//! The recording loop for Chaturbate.
//!
//! Two tracks, not one: the CDN serves a video chunklist and a separate audio
//! chunklist, each fMP4 with its own init segment. Both are appended to their
//! own file as they air, then muxed together with ffmpeg when recording stops.
//!
//! Durability rules, which exist because a live stream ends without warning:
//! - a segment is downloaded into memory and only appended once it is whole;
//! - every part starts with the `#EXT-X-MAP` init, so each file plays alone and
//!   a hard kill still leaves a decodable video;
//! - parts rotate together, driven by the video size, so part N of the video
//!   always pairs with part N of the audio;
//! - a session that expires (`w3: session_missed`) is re-minted rather than
//!   ending the recording, and the tracks resume at the live edge so nothing
//!   is written twice.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::hls;
use crate::jobs::JobContext;
use crate::site::{self, Http};
use crate::util;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    StreamEnded,
    Duration,
    Cancelled,
}

impl EndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            EndReason::StreamEnded => "stream ended",
            EndReason::Duration => "duration reached",
            EndReason::Cancelled => "stopped",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RecordOptions {
    pub model: String,
    /// `source` (default), `auto`, an exact name such as `720p`, or a
    /// resolution such as `1280x720`.
    pub quality: String,
    pub duration_secs: Option<u64>,
    pub output: Option<PathBuf>,
    pub config: Config,
}

impl RecordOptions {
    pub fn for_model(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            quality: "source".into(),
            duration_secs: None,
            output: None,
            config: Config::load(),
        }
    }
}

/// One finished part: its video and audio tracks and the MP4 they mux into.
#[derive(Debug, Clone, Default)]
pub struct PartPair {
    pub video: Option<PathBuf>,
    pub audio: Option<PathBuf>,
    pub final_path: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct RecordPaths {
    pub parts: Vec<PartPair>,
}

#[derive(Debug, Clone)]
pub struct RecordReport {
    pub paths: RecordPaths,
    pub bytes: u64,
    pub segments: u64,
    pub quality: String,
    pub resolution: String,
    pub end_reason: EndReason,
    pub seconds: u64,
    pub note: Option<String>,
}

/// Append-only part file. `create_new` means two recordings started in the
/// same second can never append into one another's parts.
struct PartWriter {
    dir: PathBuf,
    stem: String,
    unique: u32,
    role: &'static str,
    part: u32,
    file: Option<File>,
    part_bytes: u64,
    total_bytes: u64,
    paths: Vec<PathBuf>,
    current_map: Option<String>,
    init_cache: Option<(String, Vec<u8>)>,
}

impl PartWriter {
    fn new(dir: PathBuf, stem: String, role: &'static str) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let mut writer = Self {
            dir,
            stem,
            unique: 0,
            role,
            part: 0,
            file: None,
            part_bytes: 0,
            total_bytes: 0,
            paths: Vec::new(),
            current_map: None,
            init_cache: None,
        };
        writer.open_part()?;
        Ok(writer)
    }

    fn path_for(&self, part: u32) -> PathBuf {
        let stem = if self.unique == 0 {
            self.stem.clone()
        } else {
            format!("{}-{}", self.stem, self.unique)
        };
        self.dir
            .join(format!("{stem}-part{part:03}.{}.recording.mp4", self.role))
    }

    fn open_part(&mut self) -> Result<()> {
        self.part += 1;
        for _ in 0..64 {
            let path = self.path_for(self.part);
            match OpenOptions::new().create_new(true).write(true).open(&path) {
                Ok(file) => {
                    self.part_bytes = 0;
                    self.file = Some(file);
                    self.paths.push(path);
                    self.current_map = None;
                    return Ok(());
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => self.unique += 1,
                Err(e) => return Err(e.into()),
            }
        }
        Err(Error::Other("could not find a free file name".into()))
    }

    fn write(&mut self, data: &[u8]) -> Result<()> {
        if let Some(file) = self.file.as_mut() {
            file.write_all(data)?;
            file.flush()?;
            self.part_bytes += data.len() as u64;
            self.total_bytes += data.len() as u64;
        }
        Ok(())
    }

    fn ensure_init(&mut self, map: Option<&str>, bytes: &[u8]) -> Result<()> {
        let Some(uri) = map else { return Ok(()) };
        if self.current_map.as_deref() == Some(uri) {
            return Ok(());
        }
        self.write(bytes)?;
        self.current_map = Some(uri.to_string());
        Ok(())
    }

    /// Close the current part and open the next one, which starts with its own
    /// init segment so it plays alone.
    fn rotate(&mut self) -> Result<()> {
        self.file = None;
        self.open_part()
    }

    fn current_path(&self) -> Option<PathBuf> {
        self.paths.last().cloned()
    }

    /// Close the file and drop any part that never received a byte, so a
    /// rotation at the very end does not leave an empty `.recording.mp4`.
    fn finish(&mut self) {
        self.file = None;
        self.paths.retain(|path| {
            let keep = path.metadata().map(|m| m.len() > 0).unwrap_or(false);
            if !keep {
                let _ = std::fs::remove_file(path);
            }
            keep
        });
    }
}

struct Track {
    role: &'static str,
    url: String,
    writer: PartWriter,
    next_seq: Option<u64>,
    /// After a re-mint, resume at the newest listed segment: the new window
    /// overlaps what was already written, and appending the overlap would
    /// duplicate seconds of media and drift audio against video.
    resume_at_edge: bool,
}

enum Poll {
    Progress,
    Ended,
}

impl Track {
    fn new(role: &'static str, url: String, writer: PartWriter) -> Self {
        Self {
            role,
            url,
            writer,
            next_seq: None,
            resume_at_edge: false,
        }
    }

    fn retarget(&mut self, url: String) {
        if url != self.url {
            self.url = url;
            self.next_seq = None;
            self.resume_at_edge = true;
        }
    }

    fn poll(&mut self, http: &Http, ctx: &JobContext, retries: u32) -> Result<Poll> {
        let text = http
            .media_get(&self.url)
            .map_err(|e| Error::Http(format!("{} playlist: {e}", self.role)))?;
        let media = hls::parse_media_playlist(&self.url, &text)?;
        if media.segments.is_empty() {
            return Ok(Poll::Progress);
        }
        let first = media.segments[0].seq;
        let last = media.segments[media.segments.len() - 1].seq;
        let start = match self.next_seq {
            Some(next) if next > last => {
                // The CDN restarted its numbering; resume at this window.
                tracing::info!("{}: media sequence went backwards, resuming", self.role);
                first
            }
            Some(next) => next,
            None if self.resume_at_edge => {
                self.resume_at_edge = false;
                last
            }
            None => first,
        };
        self.next_seq = Some(start);

        for segment in media.segments.iter().filter(|s| s.seq >= start) {
            ctx.check_cancel().map_err(|_| Error::Cancelled(ctx.job_id()))?;
            let Some(data) = download(http, ctx, &segment.uri, retries)? else {
                return Err(Error::Http(format!(
                    "{} segment {} expired",
                    self.role, segment.seq
                )));
            };
            if let Some(map) = media.map.as_deref() {
                let init = match &self.writer.init_cache {
                    Some((uri, bytes)) if uri == map => bytes.clone(),
                    _ => {
                        let bytes = download(http, ctx, map, retries)?
                            .ok_or_else(|| Error::Http(format!("{} init expired", self.role)))?;
                        self.writer.init_cache = Some((map.to_string(), bytes.clone()));
                        bytes
                    }
                };
                self.writer.ensure_init(Some(map), &init)?;
            }
            self.writer.write(&data)?;
            self.next_seq = Some(segment.seq + 1);
            ctx.progress(self.writer.total_bytes, 0);
        }

        let consumed = self.next_seq.unwrap_or(0) >= last + 1;
        if media.end_list && consumed {
            return Ok(Poll::Ended);
        }
        Ok(Poll::Progress)
    }
}

fn download(
    http: &Http,
    ctx: &JobContext,
    url: &str,
    retries: u32,
) -> Result<Option<Vec<u8>>> {
    let mut last = None;
    for attempt in 0..=retries {
        ctx.check_cancel().map_err(|_| Error::Cancelled(ctx.job_id()))?;
        match http.media_bytes(url) {
            Ok(buf) if !buf.is_empty() => return Ok(Some(buf)),
            Ok(_) => last = Some(Error::Http("empty segment".into())),
            // An expired session, not a fatal error: the caller re-mints.
            Err(Error::Http(msg))
                if msg.contains("403") || msg.contains("404") || msg.contains("410") =>
            {
                return Ok(None)
            }
            Err(e) => last = Some(e),
        }
        if attempt < retries {
            std::thread::sleep(Duration::from_millis(350 * (attempt as u64 + 1)));
        }
    }
    Err(last.unwrap_or_else(|| Error::Http("segment download failed".into())))
}

fn part_final_path(dir: &Path, stem: &str, index: usize, total: usize) -> PathBuf {
    if total <= 1 {
        dir.join(format!("{stem}.mp4"))
    } else {
        dir.join(format!("{stem}-part{index:03}.mp4"))
    }
}

/// Record until the stream ends, the duration elapses, or the job is cancelled.
pub fn record(opts: &RecordOptions, ctx: &JobContext) -> Result<RecordReport> {
    let config = &opts.config;
    let http = Http::new(config)?;
    let started = Instant::now();
    let retries = config.segment_retries;

    let mut session = site::open(&http, &opts.model, &opts.quality)?;
    ctx.message(format!(
        "{} at {} ({}), video and audio tracks",
        opts.model, session.resolution, session.quality
    ));

    let dir = match &opts.output {
        Some(path) => path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
        None => config.resolved_output_dir(),
    };
    let stem = match &opts.output {
        Some(path) => path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("{}-{}", util::slug(&opts.model), util::timestamp())),
        None => format!("{}-{}", util::slug(&opts.model), util::timestamp()),
    };

    let mut video = Track::new(
        "video",
        session.video_url.clone(),
        PartWriter::new(dir.clone(), stem.clone(), "video")?,
    );
    let mut audio = match &session.audio_url {
        Some(url) => Some(Track::new(
            "audio",
            url.clone(),
            PartWriter::new(dir.clone(), stem.clone(), "audio")?,
        )),
        None => None,
    };

    let split = config.split_size();
    let mut finished: Vec<(Option<PathBuf>, Option<PathBuf>)> = Vec::new();
    let mut segments: u64 = 0;
    let mut end_reason = EndReason::StreamEnded;
    let mut note = None;
    let mut last_mint = Instant::now() - Duration::from_secs(60);
    let mut poll_errors: u32 = 0;

    let result: Result<()> = (|| {
        loop {
            ctx.check_cancel().map_err(|_| Error::Cancelled(ctx.job_id()))?;
            if let Some(limit) = opts.duration_secs {
                if started.elapsed() >= Duration::from_secs(limit) {
                    end_reason = EndReason::Duration;
                    return Ok(());
                }
            }

            let mut failed: Option<Error> = None;
            let mut ended = false;
            match video.poll(&http, ctx, retries) {
                Ok(Poll::Ended) => ended = true,
                Ok(Poll::Progress) => {}
                Err(Error::Cancelled(id)) => return Err(Error::Cancelled(id)),
                Err(e) => failed = Some(e),
            }
            if let Some(track) = audio.as_mut() {
                match track.poll(&http, ctx, retries) {
                    Ok(Poll::Ended) => ended = true,
                    Ok(Poll::Progress) => {}
                    Err(Error::Cancelled(id)) => return Err(Error::Cancelled(id)),
                    Err(e) => failed = failed.or(Some(e)),
                }
            }
            segments += 1;

            // Rotate both tracks together, driven by the video size, so part N
            // of the video always pairs with part N of the audio.
            if let Some(limit) = split {
                if video.writer.part_bytes >= limit {
                    finished.push((
                        video.writer.current_path(),
                        audio.as_ref().and_then(|t| t.writer.current_path()),
                    ));
                    video.writer.rotate()?;
                    if let Some(track) = audio.as_mut() {
                        track.writer.rotate()?;
                    }
                    ctx.message(format!(
                        "part {} started ({})",
                        finished.len() + 1,
                        util::format_bytes(video.writer.total_bytes)
                    ));
                }
            }

            if ended {
                end_reason = EndReason::StreamEnded;
                return Ok(());
            }

            if let Some(err) = failed {
                // An expired session is normal: mint a fresh master, at most
                // once every 10 s. Anything else gets a few free retries before
                // it is treated as fatal.
                if last_mint.elapsed() < Duration::from_secs(10) {
                    poll_errors += 1;
                    if poll_errors > 6 {
                        return Err(err);
                    }
                    std::thread::sleep(Duration::from_millis(700));
                    continue;
                }
                poll_errors = 0;
                last_mint = Instant::now();
                tracing::info!("track failure ({err}), minting a new session");
                ctx.message("session expired, minting a new one");
                match site::open(&http, &opts.model, &opts.quality) {
                    Ok(fresh) => {
                        video.retarget(fresh.video_url.clone());
                        if let (Some(track), Some(url)) = (audio.as_mut(), &fresh.audio_url) {
                            track.retarget(url.clone());
                        }
                        session = fresh;
                    }
                    Err(site_err) => match site_err {
                        // The room really is gone: stop with what we have.
                        Error::Offline(_) => {
                            end_reason = EndReason::StreamEnded;
                            return Ok(());
                        }
                        // A gated or unreachable lookup must not end a live
                        // recording; keep trying on the next tick.
                        other => {
                            note = Some(other.to_string());
                            std::thread::sleep(Duration::from_secs(2));
                        }
                    },
                }
                continue;
            }
            poll_errors = 0;

            std::thread::sleep(Duration::from_millis(900));
        }
    })();

    video.writer.finish();
    if let Some(track) = audio.as_mut() {
        track.writer.finish();
    }
    match result {
        Ok(()) => {}
        Err(Error::Cancelled(_)) => end_reason = EndReason::Cancelled,
        Err(e) => {
            if video.writer.total_bytes == 0 {
                clean_up(&video.writer, audio.as_ref().map(|t| &t.writer));
                return Err(e);
            }
            note = Some(e.to_string());
            tracing::warn!(
                "recording stopped with an error after {} bytes: {e}",
                video.writer.total_bytes
            );
        }
    }

    if video.writer.total_bytes == 0 {
        clean_up(&video.writer, audio.as_ref().map(|t| &t.writer));
        return Err(Error::Offline(
            "the room stopped before a single segment was written".into(),
        ));
    }

    finished.push((
        video.writer.current_path(),
        audio.as_ref().and_then(|t| t.writer.current_path()),
    ));
    let total = finished.len();
    let parts: Vec<PartPair> = finished
        .into_iter()
        .enumerate()
        .filter(|(_, (v, _))| v.is_some())
        .map(|(i, (video, audio))| PartPair {
            video,
            audio,
            final_path: part_final_path(&dir, &stem, i + 1, total),
        })
        .collect();

    let bytes =
        video.writer.total_bytes + audio.as_ref().map(|t| t.writer.total_bytes).unwrap_or(0);
    Ok(RecordReport {
        paths: RecordPaths { parts },
        bytes,
        segments,
        quality: session.quality.clone(),
        resolution: session.resolution.clone(),
        end_reason,
        seconds: started.elapsed().as_secs(),
        note,
    })
}

fn clean_up(video: &PartWriter, audio: Option<&PartWriter>) {
    for path in video
        .paths
        .iter()
        .chain(audio.into_iter().flat_map(|a| a.paths.iter()))
    {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!("cbr-test-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn each_track_writes_its_own_file_with_an_init() {
        let dir = TempDir::new("tracks");
        let mut video = PartWriter::new(dir.0.clone(), "stream".into(), "video").unwrap();
        let mut audio = PartWriter::new(dir.0.clone(), "stream".into(), "audio").unwrap();
        video.ensure_init(Some("i"), b"VINIT").unwrap();
        video.write(b"VSEG").unwrap();
        audio.ensure_init(Some("i"), b"AINIT").unwrap();
        audio.write(b"ASEG").unwrap();
        video.finish();
        audio.finish();

        assert_eq!(
            std::fs::read(dir.0.join("stream-part001.video.recording.mp4")).unwrap(),
            b"VINITVSEG"
        );
        assert_eq!(
            std::fs::read(dir.0.join("stream-part001.audio.recording.mp4")).unwrap(),
            b"AINITASEG"
        );
    }

    #[test]
    fn rotation_opens_a_new_part_per_track() {
        let dir = TempDir::new("rotate");
        let mut video = PartWriter::new(dir.0.clone(), "s".into(), "video").unwrap();
        video.write(b"one").unwrap();
        video.rotate().unwrap();
        video.write(b"two").unwrap();
        assert_eq!(video.paths.len(), 2);
        assert_eq!(
            std::fs::read(dir.0.join("s-part002.video.recording.mp4")).unwrap(),
            b"two"
        );
    }

    #[test]
    fn writers_never_share_a_file() {
        let dir = TempDir::new("collide");
        let a = PartWriter::new(dir.0.clone(), "same".into(), "video").unwrap();
        let b = PartWriter::new(dir.0.clone(), "same".into(), "audio").unwrap();
        assert_ne!(a.paths[0], b.paths[0], "roles must not collide");
        let c = PartWriter::new(dir.0.clone(), "same".into(), "video").unwrap();
        assert_ne!(
            a.paths[0], c.paths[0],
            "a second recording must get its own file"
        );
    }

    #[test]
    fn part_names_switch_on_the_part_count() {
        let dir = Path::new("/v");
        assert_eq!(part_final_path(dir, "s", 1, 1), PathBuf::from("/v/s.mp4"));
        assert_eq!(
            part_final_path(dir, "s", 2, 3),
            PathBuf::from("/v/s-part002.mp4")
        );
    }
}

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
//! - a session that expires (`w3: session_missed`) is re-minted rather than
//!   ending the recording: a fresh context, master and chunklists, with the
//!   same files still open.

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
    /// `source` (default), `auto`, or an exact resolution such as `1280x720`.
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

#[derive(Debug, Clone)]
pub struct RecordPaths {
    /// Video-only file (before muxing).
    pub video: PathBuf,
    pub audio: Option<PathBuf>,
    pub final_path: PathBuf,
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

/// Append-only part file with size-based rotation.
struct PartWriter {
    dir: PathBuf,
    stem: String,
    role: &'static str,
    split: Option<u64>,
    part: u32,
    file: Option<File>,
    part_bytes: u64,
    total_bytes: u64,
    paths: Vec<PathBuf>,
    current_map: Option<String>,
    init_cache: Option<(String, Vec<u8>)>,
}

impl PartWriter {
    fn new(dir: PathBuf, stem: String, role: &'static str, split: Option<u64>) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let mut writer = Self {
            dir,
            stem,
            role,
            split,
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
        if self.split.is_some() {
            self.dir
                .join(format!("{}-part{part:03}.{}.recording.mp4", self.stem, self.role))
        } else {
            self.dir
                .join(format!("{}.{}.recording.mp4", self.stem, self.role))
        }
    }

    fn open_part(&mut self) -> Result<()> {
        self.part += 1;
        let path = self.path_for(self.part);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        self.part_bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
        self.file = Some(file);
        self.paths.push(path);
        self.current_map = None;
        Ok(())
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

    fn rotate_if_needed(&mut self) -> Result<bool> {
        let Some(limit) = self.split else {
            return Ok(false);
        };
        if self.part_bytes < limit {
            return Ok(false);
        }
        self.file = None;
        self.open_part()?;
        Ok(true)
    }

    fn finish(&mut self) {
        self.file = None;
    }

    fn first_path(&self) -> Option<PathBuf> {
        self.paths.first().cloned()
    }
}

struct Track {
    role: &'static str,
    url: String,
    writer: PartWriter,
    next_seq: Option<u64>,
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
        }
    }

    /// Re-point at a freshly minted chunklist and restart from its live edge.
    fn retarget(&mut self, url: String) {
        if url != self.url {
            self.url = url;
            self.next_seq = None;
        }
    }

    fn poll(&mut self, http: &Http, ctx: &JobContext, retries: u32) -> Result<Poll> {
        let text = http.media_get(&self.url).map_err(|e| {
            Error::Http(format!("{} playlist: {e}", self.role))
        })?;
        let media = hls::parse_media_playlist(&self.url, &text)?;
        if media.segments.is_empty() {
            return Ok(Poll::Progress);
        }
        tracing::debug!(
            "{}: playlist seq={} len={} end={} map={}",
            self.role,
            media.media_sequence,
            media.segments.len(),
            media.end_list,
            media.map.is_some()
        );
        let start = *self.next_seq.get_or_insert(media.segments[0].seq);
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
            if self.writer.rotate_if_needed()? {
                ctx.message(format!("{} part {} started", self.role, self.writer.part));
            }
            self.next_seq = Some(segment.seq + 1);
            ctx.progress(self.writer.total_bytes, 0);
        }
        let consumed = self.next_seq.unwrap_or(0) >= media.media_sequence + media.segments.len() as u64;
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
            // Do not retry against a dead token.
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
        PartWriter::new(dir.clone(), stem.clone(), "video", config.split_size())?,
    );
    let mut audio = match &session.audio_url {
        Some(url) => Some(Track::new(
            "audio",
            url.clone(),
            PartWriter::new(dir.clone(), stem.clone(), "audio", config.split_size())?,
        )),
        None => None,
    };

    let mut segments: u64 = 0;
    let mut end_reason = EndReason::StreamEnded;
    let mut note = None;
    let mut last_mint = Instant::now() - Duration::from_secs(60);

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
                Err(e) => failed = Some(e),
            }
            if let Some(track) = audio.as_mut() {
                match track.poll(&http, ctx, retries) {
                    Ok(Poll::Ended) => ended = true,
                    Ok(Poll::Progress) => {}
                    Err(e) => failed = failed.or(Some(e)),
                }
                segments += 1;
            }
            segments += 1;

            if ended {
                end_reason = EndReason::StreamEnded;
                return Ok(());
            }

            if let Some(err) = failed {
                // Expired session: mint a fresh master at most every 10 s.
                if last_mint.elapsed() < Duration::from_secs(10) {
                    return Err(err);
                }
                last_mint = Instant::now();
                tracing::info!("track failure ({}), minting a new session", err);
                ctx.message("session expired, minting a new one");
                match site::open(&http, &opts.model, &opts.quality) {
                    Ok(fresh) => {
                        video.retarget(fresh.video_url.clone());
                        if let (Some(track), Some(url)) = (audio.as_mut(), &fresh.audio_url) {
                            track.retarget(url.clone());
                        }
                        session = fresh;
                    }
                    Err(site_err) => {
                        // Offline now, or the gate came back: stop with what we have.
                        if matches!(site_err, Error::Offline(_)) {
                            end_reason = EndReason::StreamEnded;
                            return Ok(());
                        }
                        return Err(site_err);
                    }
                }
                continue;
            }

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
            tracing::warn!("recording stopped with an error after {} bytes: {e}", video.writer.total_bytes);
        }
    }

    // Nothing written: a mux over empty files would fail with a confusing
    // ffmpeg error, so report the real reason.
    if video.writer.total_bytes == 0 {
        clean_up(&video.writer, audio.as_ref().map(|t| &t.writer));
        return Err(Error::Offline(
            "the room stopped before a single segment was written".into(),
        ));
    }

    let video_bytes = video.writer.total_bytes;
    let audio_bytes = audio.as_ref().map(|t| t.writer.total_bytes).unwrap_or(0);
    Ok(RecordReport {
        paths: RecordPaths {
            video: video.writer.first_path().unwrap_or_default(),
            audio: audio.as_ref().and_then(|t| t.writer.first_path()),
            final_path: dir.join(format!("{stem}.mp4")),
        },
        bytes: video_bytes + audio_bytes,
        segments,
        quality: session.quality.clone(),
        resolution: session.resolution.clone(),
        end_reason,
        seconds: started.elapsed().as_secs(),
        note,
    })
}

fn clean_up(video: &PartWriter, audio: Option<&PartWriter>) {
    for path in video.paths.iter().chain(audio.into_iter().flat_map(|a| a.paths.iter())) {
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
        let mut video = PartWriter::new(dir.0.clone(), "stream".into(), "video", None).unwrap();
        let mut audio = PartWriter::new(dir.0.clone(), "stream".into(), "audio", None).unwrap();
        video.ensure_init(Some("i"), b"VINIT").unwrap();
        video.write(b"VSEG").unwrap();
        audio.ensure_init(Some("i"), b"AINIT").unwrap();
        audio.write(b"ASEG").unwrap();
        video.finish();
        audio.finish();

        assert_eq!(
            std::fs::read(dir.0.join("stream.video.recording.mp4")).unwrap(),
            b"VINITVSEG"
        );
        assert_eq!(
            std::fs::read(dir.0.join("stream.audio.recording.mp4")).unwrap(),
            b"AINITASEG"
        );
    }

    #[test]
    fn retarget_resets_the_sequence_on_a_new_url() {
        let dir = TempDir::new("retarget");
        let writer = PartWriter::new(dir.0.clone(), "s".into(), "video", None).unwrap();
        let mut track = Track::new("video", "https://a/1.m3u8".into(), writer);
        track.next_seq = Some(99);
        track.retarget("https://a/2.m3u8".into());
        assert_eq!(track.next_seq, None);
        track.next_seq = Some(5);
        track.retarget("https://a/2.m3u8".into());
        assert_eq!(track.next_seq, Some(5), "same URL keeps the sequence");
    }
}

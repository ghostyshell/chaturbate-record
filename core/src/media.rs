//! ffmpeg use: finalize a finished recording, and optionally compress it.
//!
//! The recorder writes a fragmented MP4 that is valid even if the process is
//! killed. Finalizing remuxes it (stream copy) into a regular MP4; compressing
//! re-encodes with an x264/x265 preset modelled on Keepsake Atelier's rip
//! settings (CRF plus a speed preset, optional scale).

use crate::error::{Error, Result};
use crate::jobs::JobContext;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct Preset {
    pub id: &'static str,
    pub label: &'static str,
    pub detail: &'static str,
    /// ffmpeg output options, everything after `-i <input>`.
    pub args: &'static [&'static str],
}

pub const PRESETS: &[Preset] = &[
    Preset {
        id: "copy",
        label: "Keep original",
        detail: "Remux only. No quality loss, no waiting.",
        args: &["-c", "copy"],
    },
    Preset {
        id: "h264_balanced",
        label: "H.264 balanced",
        detail: "CRF 23, plays everywhere. About half the size.",
        args: &[
            "-c:v", "libx264", "-crf", "23", "-preset", "medium", "-pix_fmt", "yuv420p",
            "-c:a", "aac", "-b:a", "160k",
        ],
    },
    Preset {
        id: "h265_balanced",
        label: "H.265 balanced",
        detail: "CRF 26, smaller than H.264, slower to encode.",
        args: &[
            "-c:v", "libx265", "-crf", "26", "-preset", "medium", "-pix_fmt", "yuv420p",
            "-tag:v", "hvc1", "-c:a", "aac", "-b:a", "160k",
        ],
    },
    Preset {
        id: "h265_compact",
        label: "H.265 compact",
        detail: "CRF 30. Smallest files, some fine detail lost.",
        args: &[
            "-c:v", "libx265", "-crf", "30", "-preset", "medium", "-pix_fmt", "yuv420p",
            "-tag:v", "hvc1", "-c:a", "aac", "-b:a", "128k",
        ],
    },
    Preset {
        id: "h264_720",
        label: "H.264 720p",
        detail: "Scale down to 720p, CRF 24. Good for sharing.",
        args: &[
            "-vf", "scale=-2:720", "-c:v", "libx264", "-crf", "24", "-preset", "medium",
            "-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "128k",
        ],
    },
];

pub fn preset(id: &str) -> &'static Preset {
    PRESETS.iter().find(|p| p.id == id).unwrap_or(&PRESETS[0])
}

/// Candidate locations, most specific first: configured path, next to the
/// executable, the Tauri resources dir, then PATH.
pub fn find_binary(name: &str, configured: &str) -> Option<PathBuf> {
    let exe_name = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    if !configured.trim().is_empty() {
        let path = PathBuf::from(configured);
        if path.is_file() {
            return Some(path);
        }
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join(&exe_name));
            candidates.push(dir.join("resources").join(&exe_name));
            // macOS .app: Contents/MacOS/<exe> -> Contents/Resources
            if let Some(contents) = dir.parent() {
                candidates.push(contents.join("Resources").join(&exe_name));
            }
        }
    }
    candidates.push(PathBuf::from("app/src-tauri/resources").join(&exe_name));
    candidates.push(PathBuf::from("resources").join(&exe_name));
    if let Some(found) = candidates.into_iter().find(|p| p.is_file()) {
        return Some(found);
    }
    which(&exe_name)
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

pub fn ffmpeg(configured: &str) -> Option<PathBuf> {
    find_binary("ffmpeg", configured)
}

pub fn ffprobe(configured: &str) -> Option<PathBuf> {
    find_binary("ffprobe", configured)
}

fn run_meta(path: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new(path)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Error::Other(format!("{}: {e}", path.display())))?;
    if !out.status.success() {
        return Err(Error::Other(format!(
            "{} failed: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Media duration in seconds.
pub fn duration(ffprobe: &Path, path: &Path) -> Option<f64> {
    let file = path.to_str()?;
    let out = run_meta(
        ffprobe,
        &[
            "-v", "error", "-show_entries", "format=duration",
            "-of", "default=nw=1:nk=1", file,
        ],
    )
    .ok()?;
    out.trim().parse().ok()
}

/// Encode `src` to `dst`. Reports progress in bytes-per-second terms when the
/// duration is known, and kills the child if the job is cancelled.
pub fn encode(
    ffmpeg: &Path,
    src: &Path,
    dst: &Path,
    args: &[&str],
    total_secs: Option<f64>,
    cancellable: bool,
    ctx: &JobContext,
) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut child = Command::new(ffmpeg)
        .arg("-y")
        .arg("-i")
        .arg(src)
        .args(args)
        .args(["-movflags", "+faststart", "-progress", "pipe:1", "-nostats"])
        .arg(dst)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::Other(format!("ffmpeg: {e}")))?;

    use std::io::{BufRead, BufReader};
    let stdout = child.stdout.take().expect("stdout piped");
    // Drain stderr on its own thread: ffmpeg is chatty and a full pipe would
    // block the child while we wait on its progress output.
    let stderr = child.stderr.take().expect("stderr piped");
    let errors = std::thread::spawn(move || {
        let reader = BufReader::new(stderr);
        let mut tail: Vec<String> = Vec::new();
        for line in reader.lines().map_while(std::result::Result::ok) {
            tail.push(line);
            if tail.len() > 40 {
                tail.remove(0);
            }
        }
        tail.join("\n")
    });
    let reader = BufReader::new(stdout);
    let total = total_secs.filter(|t| *t > 0.0);
    let mut cancelled = false;
    for line in reader.lines() {
        if cancellable && ctx.check_cancel().is_err() {
            let _ = child.kill();
            cancelled = true;
            break;
        }
        let Ok(line) = line else { break };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key == "out_time_us" {
            if let (Ok(us), Some(total)) = (value.trim().parse::<u64>(), total) {
                let done = (us as f64 / 1_000_000.0).min(total);
                ctx.progress((done * 1000.0) as u64, (total * 1000.0) as u64);
            }
        }
    }
    let status = child.wait().map_err(|e| Error::Other(e.to_string()))?;
    let stderr_tail = errors.join().unwrap_or_default();
    if cancelled {
        let _ = std::fs::remove_file(dst);
        return Err(Error::Cancelled(ctx.job_id()));
    }
    if !status.success() {
        let _ = std::fs::remove_file(dst);
        return Err(Error::Other(format!(
            "ffmpeg exited with {status}: {}",
            stderr_tail.trim().lines().last().unwrap_or("no output")
        )));
    }
    Ok(())
}

/// Strip the track role and the `.recording` marker:
/// `x.video.recording.mp4` -> `x.mp4`.
pub fn final_path(recording: &Path) -> PathBuf {
    let name = recording
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let plain = name
        .replace(".video.recording.mp4", ".mp4")
        .replace(".audio.recording.mp4", ".mp4")
        .replace(".recording.mp4", ".mp4");
    recording.with_file_name(plain)
}

/// Mux the video and audio tracks into one file, stream copy.
pub fn mux(
    ffmpeg: &Path,
    video: &Path,
    audio: &Path,
    dst: &Path,
    ctx: &JobContext,
) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let out = Command::new(ffmpeg)
        .arg("-y")
        .arg("-i")
        .arg(video)
        .arg("-i")
        .arg(audio)
        .args(["-map", "0:v:0", "-map", "1:a:0", "-c", "copy"])
        .args(["-movflags", "+faststart"])
        .arg(dst)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Error::Other(format!("ffmpeg: {e}")))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(dst);
        return Err(Error::Other(format!(
            "mux failed: {}",
            String::from_utf8_lossy(&out.stderr)
                .trim()
                .lines()
                .last()
                .unwrap_or("no output")
        )));
    }
    let _ = ctx;
    Ok(())
}

/// Turn finished tracks into the final file.
///
/// With ffmpeg the two tracks are muxed by stream copy and, when a compression
/// preset is chosen, re-encoded in one pass from that mux. Without ffmpeg the
/// video track is renamed and the audio track is left beside it, because
/// dropping both would lose the recording.
pub fn finalize_tracks(
    ffmpeg: Option<&Path>,
    ffprobe: Option<&Path>,
    video: &Path,
    audio: Option<&Path>,
    final_path: &Path,
    preset_id: Option<&str>,
    keep_source: bool,
    ctx: &JobContext,
) -> Result<PathBuf> {
    let Some(ffmpeg) = ffmpeg else {
        std::fs::rename(video, final_path)?;
        return Ok(final_path.to_path_buf());
    };
    let size = std::fs::metadata(video).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        return Err(Error::Other("nothing was recorded to finalize".into()));
    }
    let chosen = preset(preset_id.unwrap_or("copy"));
    let total = ffprobe.and_then(|probe| duration(probe, video));

    let muxed: Option<PathBuf> = match audio {
        Some(audio_path) if chosen.id != "copy" => {
            let temp = final_path.with_extension("muxed.mp4");
            mux(ffmpeg, video, audio_path, &temp, ctx)?;
            Some(temp)
        }
        Some(audio_path) => {
            mux(ffmpeg, video, audio_path, final_path, ctx)?;
            None
        }
        None => {
            encode(ffmpeg, video, final_path, &["-c", "copy"], total, false, ctx)?;
            None
        }
    };

    if let Some(source) = muxed {
        encode(ffmpeg, &source, final_path, chosen.args, total, chosen.id != "copy", ctx)?;
        let _ = std::fs::remove_file(&source);
    }

    if !keep_source {
        let _ = std::fs::remove_file(video);
        if let Some(audio_path) = audio {
            let _ = std::fs::remove_file(audio_path);
        }
    }
    Ok(final_path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_have_unique_ids_and_copy_is_first() {
        let mut ids: Vec<&str> = PRESETS.iter().map(|p| p.id).collect();
        assert_eq!(ids[0], "copy");
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), PRESETS.len());
        assert_eq!(preset("missing").id, "copy");
    }

    #[test]
    fn final_path_drops_the_role_and_marker() {
        let p = Path::new("/videos/model-1.video.recording.mp4");
        assert_eq!(final_path(p), PathBuf::from("/videos/model-1.mp4"));
        let a = Path::new("/videos/model-1.audio.recording.mp4");
        assert_eq!(final_path(a), PathBuf::from("/videos/model-1.mp4"));
    }
}

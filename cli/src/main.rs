//! Headless CLI over `chaturbate-core`.
//!
//! Every segment is appended only once it is whole, so a Ctrl+C at any moment
//! leaves playable `.recording.mp4` tracks behind. Ctrl+C asks the job to stop
//! at the next segment and still muxes the tracks into one MP4.

use clap::{Parser, Subcommand};
use chaturbate_core::config::{self, Config};
use chaturbate_core::jobs::{set_progress_hook, JobManager, JobState};
use chaturbate_core::media;
use chaturbate_core::recorder::{self, RecordOptions};
use chaturbate_core::util::{format_bytes, format_duration};
use chaturbate_core::{doctor, library, Result};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "chaturbate-recorder",
    version,
    about = "Record Chaturbate live rooms from the CDN, video and audio"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Record a live room until it ends (or --duration elapses)
    Record {
        /// Model username, as it appears in the site URL
        model: String,
        /// Seconds to record; omit to record until the stream ends
        #[arg(long)]
        duration: Option<u64>,
        /// source (default), auto, or an exact resolution such as 1280x720
        #[arg(long, default_value = "source")]
        quality: String,
        /// Rotate to a new file at this size (1G, 512M, or 0 to disable)
        #[arg(long, default_value = "1G")]
        split_size: String,
        #[arg(long)]
        output_dir: Option<PathBuf>,
        /// Exact output path, overrides --output-dir
        #[arg(long)]
        output: Option<PathBuf>,
        /// After recording, compress with this preset
        #[arg(long)]
        convert: Option<String>,
        /// Keep the intermediate track files
        #[arg(long)]
        keep_source: bool,
        /// Proxy for the room lookup, e.g. socks5h://127.0.0.1:9050
        #[arg(long)]
        proxy: Option<String>,
    },
    /// Convert or compress an existing file
    Convert {
        file: PathBuf,
        #[arg(long, default_value = "h264_balanced")]
        preset: String,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// List the presets available to `convert`
    Presets,
    /// List recorded files
    Library {
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Check ffmpeg, the output folder and the room API
    Doctor,
}

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "chaturbate_core=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    match run(Cli::parse()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn install_ctrlc() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    let handler_flag = flag.clone();
    // First Ctrl+C asks the job to stop at the next segment; a second one
    // exits immediately, because ctrlc replaces the default disposition.
    let _ = ctrlc::set_handler(move || {
        if handler_flag.swap(true, Ordering::SeqCst) {
            eprintln!("\n  force quit");
            std::process::exit(130);
        }
    });
    flag
}

fn wait(manager: &JobManager, id: u64, interrupted: &AtomicBool) {
    while manager
        .get(id)
        .map(|j| j.state == JobState::Running)
        .unwrap_or(false)
    {
        if interrupted.swap(false, Ordering::SeqCst) {
            eprintln!("\n  stopping at the next segment and muxing ...");
            manager.cancel(id);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn run(cli: Cli) -> Result<()> {
    let mut cfg = Config::load();
    let interrupted = install_ctrlc();
    match cli.command {
        Command::Record {
            model,
            duration,
            quality,
            split_size,
            output_dir,
            output,
            convert,
            keep_source,
            proxy,
        } => {
            if let Some(dir) = output_dir {
                cfg.output_dir = dir.to_string_lossy().into_owned();
            }
            if let Some(proxy) = proxy {
                cfg.proxy = proxy;
            }
            cfg.split_size_bytes = if matches!(
                split_size.to_ascii_lowercase().as_str(),
                "0" | "none" | "off" | "no"
            ) {
                0
            } else {
                config::parse_size(&split_size).ok_or_else(|| {
                    chaturbate_core::Error::Other(format!("bad --split-size {split_size:?}"))
                })?
            };

            let opts = RecordOptions {
                model: model.clone(),
                quality,
                duration_secs: duration,
                output,
                config: cfg.clone(),
            };
            let manager = Arc::new(JobManager::new());
            let hook_manager = manager.clone();
            set_progress_hook(Some(Arc::new(move |id, done, total| {
                if total == 0
                    && hook_manager
                        .get(id)
                        .map(|j| j.state == JobState::Running)
                        .unwrap_or(false)
                {
                    eprint!("\r  recorded {}", format_bytes(done));
                }
            })));

            let job_config = cfg.clone();
            let id = manager.spawn("record", &model, |_| {}, move |ctx| {
                let report = recorder::record(&opts, ctx)?;
                ctx.message(format!(
                    "{} in {} ({} chunk poll(s), {})",
                    format_bytes(report.bytes),
                    format_duration(report.seconds),
                    report.segments,
                    report.end_reason.as_str()
                ));
                let ffmpeg = media::ffmpeg(&job_config.ffmpeg_path);
                let ffprobe = media::ffprobe(&job_config.ffprobe_path);
                let preset = convert.as_deref().or(if job_config.convert_on_finish {
                    Some(job_config.convert_preset.as_str())
                } else {
                    None
                });
                let written = media::finalize_parts(
                    ffmpeg.as_deref(),
                    ffprobe.as_deref(),
                    &report.paths.parts,
                    preset,
                    keep_source || job_config.keep_ts,
                    ctx,
                )?;
                ctx.message(format!("saved {} file(s)", written.len()));
                if let Some(note) = &report.note {
                    eprintln!("\n  note: {note}");
                }
                Ok(())
            });

            wait(&manager, id, &interrupted);
            eprintln!();
            let job = manager.get(id).expect("job");
            if job.state == JobState::Done {
                println!("{}", job.message);
            } else {
                return Err(chaturbate_core::Error::Other(job.message));
            }
        }
        Command::Convert {
            file,
            preset,
            output,
        } => {
            let ffmpeg = media::ffmpeg(&cfg.ffmpeg_path).ok_or_else(|| {
                chaturbate_core::Error::Other(
                    "ffmpeg not found; install it or set the path in Settings".into(),
                )
            })?;
            let ffprobe = media::ffprobe(&cfg.ffprobe_path);
            let out = output.unwrap_or_else(|| {
                let stem = file
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "output".into());
                file.with_file_name(format!("{stem}.{preset}.mp4"))
            });
            let manager = JobManager::new();
            let out_for_job = out.clone();
            let label = file.display().to_string();
            let id = manager.spawn("convert", &label, |_| {}, move |ctx| {
                let total = ffprobe
                    .as_deref()
                    .and_then(|probe| media::duration(probe, &file));
                let chosen = media::preset(&preset);
                media::encode(&ffmpeg, &file, &out_for_job, chosen.args, total, true, ctx)
            });
            wait(&manager, id, &interrupted);
            let job = manager.get(id).expect("job");
            if job.state == JobState::Done {
                println!("saved {}", out.display());
            } else {
                return Err(chaturbate_core::Error::Other(job.message));
            }
        }
        Command::Presets => {
            for preset in media::PRESETS {
                println!("{:16} {:22} {}", preset.id, preset.label, preset.detail);
            }
        }
        Command::Library { dir } => {
            let dir = dir.unwrap_or_else(|| cfg.resolved_output_dir());
            for row in library::list(&dir)? {
                let marker = if row.in_progress { " (recording)" } else { "" };
                println!("{:<10} {}{}", format_bytes(row.size_bytes), row.name, marker);
            }
        }
        Command::Doctor => {
            let mut all_ok = true;
            for check in doctor::run(&cfg) {
                all_ok &= check.ok;
                println!(
                    "{} {:<16} {}{}",
                    if check.ok { "ok  " } else { "FAIL" },
                    check.name,
                    check.detail,
                    if check.ok || check.fix.is_empty() {
                        String::new()
                    } else {
                        format!("\n     fix: {}", check.fix)
                    }
                );
            }
            if !all_ok {
                return Err(chaturbate_core::Error::Other("one or more checks failed".into()));
            }
        }
    }
    Ok(())
}

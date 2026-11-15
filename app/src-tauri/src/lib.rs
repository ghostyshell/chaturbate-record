//! Tauri command layer over `chaturbate-core`.
//!
//! The frontend calls these with `invoke(...)`. Long work runs on the job
//! queue and reports through `job://progress` and `job://done`.

use chaturbate_core::config::Config;
use chaturbate_core::jobs::{self, JobManager};
use chaturbate_core::library::{self, Recording};
use chaturbate_core::media;
use chaturbate_core::recorder::{self, RecordOptions};
use chaturbate_core::{doctor, site, Error, Result};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, State};

pub struct AppState {
    jobs: JobManager,
    config: Mutex<Config>,
}

impl AppState {
    fn new() -> Self {
        Self {
            jobs: JobManager::new(),
            config: Mutex::new(Config::load()),
        }
    }

    fn config(&self) -> Config {
        self.config.lock().expect("config lock").clone()
    }
}

#[derive(Serialize)]
pub struct Snapshot {
    config: Config,
    jobs: Vec<jobs::Job>,
    doctor: Vec<doctor::Check>,
    ffmpeg: bool,
    output_dir: String,
    version: String,
}

#[tauri::command]
fn snapshot(state: State<'_, AppState>) -> Snapshot {
    let config = state.config();
    Snapshot {
        output_dir: config.resolved_output_dir().to_string_lossy().into_owned(),
        ffmpeg: media::ffmpeg(&config.ffmpeg_path).is_some(),
        doctor: doctor::run(&config),
        jobs: state.jobs.list(),
        config,
        version: env!("CARGO_PKG_VERSION").to_string(),
    }
}

#[derive(Serialize)]
pub struct CamView {
    model: String,
    live: bool,
    room_title: Option<String>,
    variants: Vec<VariantView>,
    message: String,
}

#[derive(Serialize)]
pub struct VariantView {
    name: String,
    resolution: String,
    bandwidth: u64,
}

/// Look a room up without recording it: is it live, and at what qualities.
#[tauri::command]
fn cam_info(state: State<'_, AppState>, model: String) -> CamView {
    let config = state.config();
    let model = model.trim().to_string();
    let empty = |message: String| CamView {
        model: model.clone(),
        live: false,
        room_title: None,
        variants: Vec::new(),
        message,
    };
    let http = match site::Http::new(&config) {
        Ok(http) => http,
        Err(err) => return empty(err.to_string()),
    };
    let ctx = match site::fetch_context(&http, &model) {
        Ok(ctx) => ctx,
        Err(err) => return empty(err.to_string()),
    };
    if !ctx.is_public_live() {
        return empty(format!(
            "Offline ({})",
            ctx.room_status.as_deref().unwrap_or("unknown")
        ));
    }
    let master_url = match ctx.hls_source.clone() {
        Some(url) => url,
        None => return empty("no hls_source in the room context".into()),
    };
    let text = match http.media_get(&master_url) {
        Ok(text) => text,
        Err(err) => return empty(err.to_string()),
    };
    let master = site::parse_master(&master_url, &text);
    CamView {
        model,
        live: true,
        room_title: ctx.room_title.clone(),
        message: format!(
            "Live at {} video variant(s), {} audio rendition(s)",
            master.variants.len(),
            master.audio.len()
        ),
        variants: master
            .variants
            .into_iter()
            .map(|v| VariantView {
                name: v.name.unwrap_or_else(|| "auto".into()),
                resolution: v
                    .resolution
                    .map(|(w, h)| format!("{w}x{h}"))
                    .unwrap_or_else(|| "source".into()),
                bandwidth: v.bandwidth,
            })
            .collect(),
    }
}

fn emit_progress(app: &AppHandle, id: u64, done: u64, total: u64) {
    let _ = app.emit(
        "job://progress",
        serde_json::json!({ "id": id, "done": done, "total": total }),
    );
}

fn spawn_record(
    app: &AppHandle,
    state: &AppState,
    opts: RecordOptions,
    convert: Option<String>,
    keep_source: bool,
) -> u64 {
    let config = opts.config.clone();
    let label = opts.model.clone();
    let app_progress = app.clone();
    state.jobs.spawn(
        "record",
        &label,
        move |job| {
            let _ = app_progress.emit("job://done", job);
        },
        move |ctx| {
            let report = recorder::record(&opts, ctx)?;
            ctx.message(format!(
                "{} in {} ({} poll(s), {})",
                chaturbate_core::util::format_bytes(report.bytes),
                chaturbate_core::util::format_duration(report.seconds),
                report.segments,
                report.end_reason.as_str()
            ));
            let ffmpeg = media::ffmpeg(&config.ffmpeg_path);
            let ffprobe = media::ffprobe(&config.ffprobe_path);
            let preset = convert.as_deref().or(if config.convert_on_finish {
                Some(config.convert_preset.as_str())
            } else {
                None
            });
            let out = media::finalize_tracks(
                ffmpeg.as_deref(),
                ffprobe.as_deref(),
                &report.paths.video,
                report.paths.audio.as_deref(),
                &report.paths.final_path,
                preset,
                keep_source || config.keep_ts,
                ctx,
            )?;
            ctx.message(format!("saved {}", out.display()));
            Ok(())
        },
    )
}

#[tauri::command]
fn record_start(
    app: AppHandle,
    state: State<'_, AppState>,
    model: String,
    quality: Option<String>,
    duration_secs: Option<u64>,
    convert: Option<String>,
    keep_source: Option<bool>,
) -> Result<u64> {
    let opts = RecordOptions {
        model: model.trim().to_string(),
        quality: quality.unwrap_or_else(|| "source".into()),
        duration_secs,
        output: None,
        config: state.config(),
    };
    Ok(spawn_record(&app, &state, opts, convert, keep_source.unwrap_or(false)))
}

#[tauri::command]
fn convert_start(
    app: AppHandle,
    state: State<'_, AppState>,
    file: String,
    preset: String,
    output: Option<String>,
) -> Result<u64> {
    let config = state.config();
    let ffmpeg = media::ffmpeg(&config.ffmpeg_path)
        .ok_or_else(|| Error::other("ffmpeg not found; set its path in Settings"))?;
    let ffprobe = media::ffprobe(&config.ffprobe_path);
    let src = PathBuf::from(&file);
    if !src.is_file() {
        return Err(Error::other(format!("no such file: {file}")));
    }
    let dst = output.map(PathBuf::from).unwrap_or_else(|| {
        let stem = src
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "output".into());
        src.with_file_name(format!("{stem}.{preset}.mp4"))
    });
    let label = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let app_progress = app.clone();
    Ok(state.jobs.spawn(
        "convert",
        &label,
        move |job| {
            let _ = app_progress.emit("job://done", job);
        },
        move |ctx| {
            let total = ffprobe
                .as_deref()
                .and_then(|probe| media::duration(probe, &src));
            let chosen = media::preset(&preset);
            media::encode(&ffmpeg, &src, &dst, chosen.args, total, true, ctx)?;
            ctx.message(format!("saved {}", dst.display()));
            Ok(())
        },
    ))
}

#[tauri::command]
fn library_list(state: State<'_, AppState>) -> Result<Vec<Recording>> {
    library::list(&state.config().resolved_output_dir())
}

#[tauri::command]
fn library_delete(path: String) -> Result<()> {
    library::delete(&PathBuf::from(path))
}

#[tauri::command]
fn library_reveal(path: String) -> Result<()> {
    library::reveal(&PathBuf::from(path))
}

#[tauri::command]
fn config_get(state: State<'_, AppState>) -> Config {
    state.config()
}

#[tauri::command]
fn config_set(state: State<'_, AppState>, patch: serde_json::Value) -> Result<Config> {
    let merged = {
        let mut base = serde_json::to_value(state.config())?;
        if let (Some(base), Some(patch)) = (base.as_object_mut(), patch.as_object()) {
            for (key, value) in patch {
                base.insert(key.clone(), value.clone());
            }
        }
        serde_json::from_value::<Config>(base)?
    };
    merged.save()?;
    *state.config.lock().expect("config lock") = merged.clone();
    Ok(merged)
}

#[tauri::command]
fn doctor_run(state: State<'_, AppState>) -> Vec<doctor::Check> {
    doctor::run(&state.config())
}

#[tauri::command]
fn jobs_list(state: State<'_, AppState>) -> Vec<jobs::Job> {
    state.jobs.list()
}

#[tauri::command]
fn job_cancel(state: State<'_, AppState>, id: u64) -> bool {
    state.jobs.cancel(id)
}

#[tauri::command]
fn jobs_clear(state: State<'_, AppState>) {
    state.jobs.clear_finished();
}

#[tauri::command]
fn open_output_dir(state: State<'_, AppState>) -> Result<String> {
    let dir = state.config().resolved_output_dir();
    std::fs::create_dir_all(&dir)?;
    library::reveal(&dir)?;
    Ok(dir.to_string_lossy().into_owned())
}

pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "chaturbate_core=info".into()),
        )
        .init();

    let state = AppState::new();
    let progress_app: Arc<Mutex<Option<AppHandle>>> = Arc::new(Mutex::new(None));
    let hook_holder = progress_app.clone();
    jobs::set_progress_hook(Some(Arc::new(move |id, done, total| {
        if let Some(app) = hook_holder.lock().expect("app lock").as_ref() {
            emit_progress(app, id, done, total);
        }
    })));

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_process::init())
        .manage(state)
        .setup(move |app| {
            *progress_app.lock().expect("app lock") = Some(app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            cam_info,
            record_start,
            convert_start,
            library_list,
            library_delete,
            library_reveal,
            config_get,
            config_set,
            doctor_run,
            jobs_list,
            job_cancel,
            jobs_clear,
            open_output_dir,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Chaturbate Recorder");
}

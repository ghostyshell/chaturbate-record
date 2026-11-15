//! Chaturbate Recorder engine.
//!
//! The app records a live room straight from the CDN instead of capturing a
//! screen: the room context names an `hls_source` master, the master lists
//! video variants plus separate audio renditions, and both chunklists carry
//! plain fMP4 fragments that append into files. No browser, no re-encode.
//!
//! Modules:
//! - `site`: Chaturbate API, cookie jar, master and chunklist requests
//! - `hls`: playlist parsing and variant choice
//! - `recorder`: the two-track recording loop and the append-only writer
//! - `media`: ffmpeg mux and compression presets
//! - `jobs`: background queue with progress and cancel
//! - `library`, `doctor`, `config`, `util`

pub mod config;
pub mod doctor;
pub mod error;
pub mod hls;
pub mod jobs;
pub mod library;
pub mod media;
pub mod recorder;
pub mod site;
pub mod util;

pub use config::Config;
pub use error::{Error, Result};
pub use recorder::{EndReason, RecordOptions, RecordReport};

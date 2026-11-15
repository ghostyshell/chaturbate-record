# Changelog

All notable changes to **chaturbate-record** are listed here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.5.0] - 2026-11-15

### Added
- Tauri 2 + Rust desktop app for macOS, Windows and Linux, replacing the
  Python Playwright recorder. React frontend with Record, Library, Convert and
  Settings pages, a job queue with progress and cancel, and a `?mock=1`
  fixture bridge for UI work without a network.
- `chaturbate-core` engine: room context, cookies, single-use master, two-track fMP4 recording
- `chaturbate-recorder-cli` headless CLI: `record`, `convert`, `presets`, `library`,
  `doctor`.
- Recording is append-only and per segment, so a dropped connection or a hard
  kill leaves a playable `.recording.mp4` instead of a corrupt file.
- Size-based splitting into `-partNNN` files, each with its own init segment.
- Conversion presets (H.264 / H.265 CRF, 720p downscale) modelled on Keepsake
  Atelier's rip settings, plus a lossless remux.
- Site page at https://ghostyshell.github.io/chaturbate-record/, guides, AGENTS.md and DESIGN.md.

### Fixed
- Split parts are muxed pairwise, video part N with audio part N; before this only
  the first part was finalized and the rest were orphaned on disk.
- A re-mint resumes at the live edge, so overlapping window segments are no longer
  appended twice and the tracks stay in sync.
- Pressing Stop with compression enabled keeps the finished file instead of
  discarding it, and no longer leaves a `.muxed.mp4` temp behind.
- Audio renditions are found regardless of attribute order, relative audio URIs
  resolve against the master, and a zero-byte audio track is treated as absent
  instead of failing the mux.
- The age gate rotates the SOCKS circuit instead of giving up on the first try, and
  rotation preserves an authenticated proxy.
- Deleting from the Library only touches finished videos inside the recordings
  folder, never a track a running job is writing.
- Two recordings started in the same second no longer share one file.
- Media and playlist fetches reject non-CDN hosts, URL userinfo tricks and
  redirects.
- Settings are written atomically, so a kill mid-save cannot silently reset them.
- The CLI exits on a second Ctrl+C.

### Removed
- The Python recorder, `webrtc_client.html`, `run.sh` / `run.bat` and the
  Playwright/Chromium dependency.

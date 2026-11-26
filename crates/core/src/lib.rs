//! ClipVault core library.
//!
//! Everything that both the background service and the GUI need lives here:
//! configuration, the library database, OBS control (websocket + config files),
//! ffmpeg tooling, and the pure decision logic (game detection, retention,
//! mic selection, timeline math) that is unit tested in isolation.

pub mod audio_names;
pub mod config;
pub mod db;
pub mod ffmpeg;
pub mod games;
pub mod hotkey;
pub mod ini;
pub mod ipc;
pub mod logging;
pub mod mixer;
pub mod model;
pub mod naming;
pub mod obs;
pub mod paths;
pub mod retention;
pub mod timeline;
pub mod util;
pub mod waveform;

pub const APP_NAME: &str = "ClipVault";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

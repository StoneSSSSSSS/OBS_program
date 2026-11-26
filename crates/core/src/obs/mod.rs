//! Controlling OBS Studio: the obs-websocket v5 client, and reading/writing
//! OBS's own configuration files while OBS is not running.

pub mod client;
pub mod files;
pub mod filename;
pub mod protocol;

pub use client::{ObsClient, ObsEvent};

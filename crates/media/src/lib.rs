//! ClipVault's media engine.
//!
//! Everything ClipVault used OBS, ffmpeg and mpv for, built on what ships with
//! Windows 10/11: DXGI desktop duplication for the screen, WASAPI (including
//! per-application loopback) for sound, Media Foundation for H.264/AAC
//! encoding and decoding, and our own MP4 reader/writer.

pub mod aac;
pub mod audioread;
pub mod capture;
pub mod clock;
pub mod cut;
pub mod d3d;
pub mod export;
pub mod h264;
pub mod mf;
pub mod mp4;
pub mod probe;
pub mod recorder;
pub mod thumb;
pub mod util;
pub mod wave;

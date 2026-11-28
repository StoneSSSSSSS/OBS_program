//! ClipVault's media engine.
//!
//! Everything ClipVault used OBS, ffmpeg and mpv for, built on what ships with
//! Windows 10/11: DXGI desktop duplication for the screen, WASAPI (including
//! per-application loopback) for sound, Media Foundation for H.264/AAC
//! encoding and decoding, and our own MP4 reader/writer.

pub mod aac;
pub mod capture;
pub mod d3d;
pub mod h264;
pub mod mf;
pub mod mp4;

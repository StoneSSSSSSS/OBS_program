# ClipVault

Records everything, clips anything. ClipVault keeps OBS Studio recording 24/7 in the background,
saves clips with a hotkey (filed by game automatically), and gives you a fast desktop app to browse
clips, scrub back through the last couple of days of footage, rebalance separate audio tracks
(game / voice chat / music / mic), and cut new clips from the recording. It's non-destructive, so the
original audio is always kept.

Windows 10/11 only. Written in Rust.

## What it does

- **Background service** (`clipvault-service.exe`, starts at login, tray icon):
  - launches OBS minimised and keeps the 24/7 recording and replay buffer running; restarts OBS if it
    crashes and restarts the recording if it stalls;
  - **clip hotkey** (default `Ctrl+Shift+\``) saves the replay buffer, shows your overlay image, and
    files the clip under the game you were playing (detected from the program, not the window
    title, so anti-cheat title tricks and Chrome tabs don't create junk folders);
  - **6 audio tracks** in every recording: full mix, game, voice chat, music, microphone, desktop.
    The game track follows whichever game you're playing;
  - **mic guard**: matches your microphone by name, so when a USB headset reconnects under a new
    Windows device ID, OBS is switched to it within seconds instead of recording silence;
  - **retention** keeps the last *N hours of footage* (time with the PC off doesn't count), plus an
    optional age limit, storage cap and minimum free space; "kept" files are never deleted;
  - background jobs: clip from the 24/7 recording, trim, export to a size limit (e.g. Discord's 10 MB),
    with the result copied to the clipboard; waveform generation for the audio lanes.
- **App** (`clipvault.exe`):
  - **Home**: recording, replay buffer, mic and disk status; live audio meters; recent clips; activity log;
  - **24/7 Recordings**: zoomable timeline across all recording files, an activity lane showing what was
    on screen, markers, existing clips, *Go live*, drag-to-scrub, In/Out selection, and *Create clip*;
  - **Audio lanes** (audio button next to the speed menu): waveform, mute and volume per track; drag across
    a track to select part of it and mute it or make it louder. Edits are metadata only and are carried
    into clips you create;
  - **Clips**: library grid with search, games, custom categories, favourites and bulk actions; clip view
    with trim, export/share, notes and the same audio lanes;
  - **Settings**: about 100 options (recording quality, retention, clip length, hotkeys including mouse
    side buttons, overlay, mic priority, voice/music apps, games, player, export, OBS, advanced).

## Install / update

Requirements: OBS Studio 30+ (31 tested), an ffmpeg 5+ build (the installer bundles one from
`C:\Program Files\ffmpeg*`), Rust (MSVC toolchain), and `libmpv-2.dll`.

```powershell
# one-time: get libmpv (Windows build linked from mpv.io)
#   https://github.com/shinchiro/mpv-winbuild-cmake/releases  ->  mpv-dev-x86_64-*.7z
#   extract libmpv-2.dll into third_party\
cargo build --release
powershell -ExecutionPolicy Bypass -File scripts\install.ps1
```

The installer copies everything to `%LOCALAPPDATA%\Programs\ClipVault`, replaces the old Python
program's Startup entry with **ClipVault Service**, adds Start Menu/Desktop shortcuts and restarts the
service. OBS keeps recording throughout. Re-run it after pulling changes.

## Where things are

| What | Where |
|---|---|
| Settings | `%LOCALAPPDATA%\ClipVault\config.toml` (edit in the app) |
| Library database | `%LOCALAPPDATA%\ClipVault\library.db` (SQLite) |
| Logs | `%LOCALAPPDATA%\ClipVault\logs\` |
| Thumbnails / waveforms | `%LOCALAPPDATA%\ClipVault\thumbs\`, `waves\` |
| Recordings / clips | configured folders (defaults taken from OBS: `I:\OBS\Recordings`, `I:\OBS\Clips`) |

ClipVault writes OBS's output settings (format, tracks, encoder, replay length) only while OBS is
closed, and keeps one-time backups next to the originals (`*.clipvault-backup`). Turn this off under
Settings → OBS → *Let ClipVault manage OBS output settings*.

## Project layout

```
crates/core      shared library: config, DB, OBS websocket + config files, ffmpeg, mixer,
                 waveforms, game detection, retention, IPC protocol (unit tested)
crates/service   background service: OBS supervisor, Core Audio mic guard, activity tracker,
                 clip pipeline, library/retention, jobs, waveforms, tray/overlay/hotkeys
crates/gui       egui app with an embedded libmpv player (OpenGL render API)
scripts/         install.ps1, icon generator
assets/          overlay_img.png (your overlay), icons
```

Run the tests with `cargo test --workspace`. Some tests run a real ffmpeg if one is installed.

Useful service commands: `clipvault-service.exe --diagnose` (audio devices, OBS state, JSON),
`--stop`, `--migrate <old program folder>`.

See `TODO.md` for what's still planned.

# ClipVault: status & remaining work

Last updated 2026-11-25 (end of the first build session). Branch: `rust-rewrite`.

## How to pick this up (for the next agent)
1. Read `README.md` (features, layout, paths) and this file.
2. Build and test: `cargo test --workspace` (101 tests; some use the real ffmpeg at
   `C:\Program Files\ffmpeg-6.0-essentials_build\bin`).
3. Install over the running copy: `cargo build --release`, then
   `powershell -ExecutionPolicy Bypass -File scripts\install.ps1`. The installer stops and restarts the service and
   GUI (it reopens the window if it was open). OBS keeps recording.
4. `third_party\libmpv-2.dll` is gitignored (122 MB). On another machine, get `mpv-dev-x86_64-*.7z` from
   https://github.com/shinchiro/mpv-winbuild-cmake/releases and extract `libmpv-2.dll` into `third_party\`.
5. Logs: `%LOCALAPPDATA%\ClipVault\logs\service.log.*` and `gui.log.*`.
   `clipvault-service.exe --diagnose` prints audio devices, OBS/websocket state, and pending settings.
6. Debug hooks: env `CLIPVAULT_MPV_LOG=<file>` writes mpv's verbose log. Env `CLIPVAULT_SELFTEST=<dir>` makes the
   GUI screenshot each page to `<dir>` and exit. The user asked for **no more screenshot testing** in this
   session, and declined computer-use screen control.
7. The user's standing instruction: keep improving continuously and push to GitHub regularly. Keep this file current.

## User context worth knowing
- "M drive" = `I:` (volume label "Fast M2"). Recordings go to `I:\OBS\Recordings`, clips to `I:\OBS\Clips`.
- Mic: Astro A50. Windows re-registers it as "2- …", "3- …" on reconnect, which is why the mic guard matches by name.
  Windows' default output is the A50 *Voice* channel (Discord plays there).
- The user loves their `assets/overlay_img.png`. **Never change it.**
- Retention was switched from "keep N recordings" to "keep N hours of footage" (48 h), so nothing older was
  deleted at the switch. An optional age limit is off by default.
- The old Python program's files are still in the repo folder, untracked (see `.git/info/exclude`), and are no
  longer started at login. They can be deleted once the user is happy.

## Open decisions (ask the user)
- **YouTube/browser audio**: apps not on the Voice/Music lists are only in the Full mix and Desktop tracks, so in
  "Separate tracks" mode YouTube is silent unless Desktop is raised. Settings → Audio now has a one-click
  "Add browsers & media players" (puts Chrome/Edge/Firefox/… on the Music track). Should that be the default,
  and should the track be renamed "Music & media"?
- Merge `rust-rewrite` into `main` (or open a PR) once the user has used it for a bit.

## Verify with the user (built, not yet confirmed by them)
- Video playback in the clip and 24/7 views. The multi-track mixing bug was found and fixed (canonical-order
  `join`). Not yet seen working by the user after the fix.
- Timeline scrubbing feel, audio lanes, region edits (mute/louder/resize), game sessions list, "Show in 24/7".
- Hotkey capture in Settings (the service suspends global hotkeys during capture).

## Next improvements (ideas, in rough priority order)
1. Fades at audio-edit boundaries. `volume` with `eval=frame` switches per audio frame (~21 ms), so hard mutes can
   click. For exports, consider `afade`/per-sample `aeval`; for playback, a short ramp across a few frames.
2. Waveform backfill: each 1 h file (~13 GB) is read once at ≤50x realtime and paused while a game is focused.
   Watch CPU/disk impact. Waveforms for 24/7 files appear newest first.
3. Clip view: zoom/pan on the clip's own timeline for long clips (7-minute replays), like the 24/7 view.
4. Export straight from a 24/7 range (skip creating a clip first). Share presets (Discord 10 MB / 50 MB / 500 MB).
5. "Undo" for the last audio edit (keep a small history in the views).
6. Home page: small timeline preview of the last hour; quick "clip last 30 s from 24/7".
7. Optional: hide the Desktop track from the mixer when it isn't useful; per-clip track renaming.
8. Code hygiene: some GUI files are large (`views/recordings.rs`, `views/clips.rs`, `views/settings.rs`); split
   them into smaller modules. Add GUI-free unit tests for the timeline math (`x_of`/`t_of`, zoom anchoring).

## Done
- **Core** (`crates/core`): config with ~100 settings, SQLite library (schema v2 with audio edits, auto-migrating),
  OBS websocket v5 client, OBS config writer (dry-run aware, backups), ffmpeg probe/thumbnail/cut/export, gain
  expressions for mixing and region edits, waveform generator, game detection (Steam/Epic/Riot folders, known
  games, full-screen), footage-hours retention, mic matching by name, hotkey parsing, timeline math, IPC protocol.
- **Service** (`crates/service`):
  - OBS: launches and supervises OBS (crash restarts, restart limit, stall watchdog) and applies settings.
  - Audio: 6-track routing with game audio following the focused game, and apps playing on other devices
    added to the full mix. The mic guard uses Core Audio notifications.
  - Clips: activity tracking, clip hotkey with overlay, filing by game, stray-replay import, and a one-time tidy of
    the old clip folders (`%LOCALAPPDATA%\ClipVault\migration.log`).
  - Background work: retention, jobs (clip from range / trim / export with edits), waveform worker (throttled,
    pauses while gaming), tray and toasts, local IPC, hotkey suspend with a 30 s safety resume.
- **GUI** (`crates/gui`):
  - Pages: Home; 24/7 timeline (scrub, pan/zoom, activity lane, markers, clips, game sessions, Go live,
    create clip); Clips library (search, games, categories, favourites, bulk actions); clip view (trim,
    share/export, details, notes, Show in 24/7); Settings (12 sections).
  - Audio lanes with waveforms, per-track mute/volume, and non-destructive region edits. Controls sit in
    bottom panels, so fullscreen works.
- **Install**: `scripts/install.ps1`; installed at `%LOCALAPPDATA%\Programs\ClipVault`, with
  "ClipVault Service" in Startup, plus Start Menu and Desktop shortcuts.

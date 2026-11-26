# ClipVault: remaining work

Status as of 2026-11-25. The Rust rewrite (core library, background service, GUI) builds
and runs on this PC. The service is managing OBS and recording 6 audio tracks. The items
below are what's left, in priority order.

## 1. Install on this PC (done 2026-11-25)
- [x] `scripts/install.ps1` (run after `cargo build --release`): installs to `%LOCALAPPDATA%\Programs\ClipVault\`
      with libmpv, bundled ffmpeg 6.0 and assets; removes the old `main.py` Startup shortcut; creates
      `ClipVault Service.lnk` (Startup), plus Start Menu and Desktop shortcuts; restarts the service, which
      adopts the running OBS.
- To update after code changes: `cargo build --release`, then re-run `scripts\install.ps1`.
- `third_party\libmpv-2.dll` is gitignored (122 MB). On a fresh machine, get `mpv-dev-x86_64-*.7z` from
  https://github.com/shinchiro/mpv-winbuild-cmake/releases and extract `libmpv-2.dll` into `third_party\`.
- The old Python files are still in the repo folder, untracked (listed in `.git/info/exclude`) and no longer
  started at login. They can be deleted.

## 2. 24/7 timeline & player (user feedback): implemented, needs the user's eyes
- [x] Audio-tracks button toggles **audio lanes** under the timeline (per track: name, mute, volume, waveform).
- [x] Drag on the timeline to **scrub** (keyframe seeks while dragging, exact on release, resumes playback).
      Drag the time ruler or right-drag to pan; wheel zooms; Shift+wheel pans; auto-pans at the edges.
      Bigger playhead handle.
- [x] Controls live in a bottom panel, so fullscreen and small windows never cut them off (24/7 and clip views).
- [x] Marker labels truncate; the rename box has a fixed width and char limit (no more layout blow-up).
- [x] Activity lane labelled ("what was on screen"), app names drawn in wide blocks, alt-tab blips merged.
- [ ] Verify with the user (they asked for no more automated screenshot testing).

## 3. Non-destructive audio region edits: implemented
- [x] Core `AudioEdit`, `apply_edit`/`rebase_edits`, DB schema v2 `clips.audio_edits` (auto-migrates v1).
- [x] Per-track gain expressions (`volume=eval=frame:volume='g*if(between(t,a,b),x,1)…'`) in mpv `af` and ffmpeg export.
- [x] Lanes: drag to select, then Mute / −6 dB / +6 dB / exact % / Reset, with an "All tracks" option.
      Click an edit to select it. Editing a stem while in full-mix mode switches to separate tracks.
- [x] 24/7 edits persist in `%LOCALAPPDATA%\ClipVault	imeline_edits.json` (pruned with old footage).
- [x] Create clip carries the track volumes plus the edits inside the range into the clip's metadata;
      the file keeps all original tracks. Trim and export honour the edits.
- [ ] Ideas: drag edges of an existing edit to resize; fades at edit boundaries (avoid clicks on hard mutes).

## 4. Waveforms: implemented
- [x] Service `waves.rs`: newest recordings first, then clips; the live file is extended every ~60 s;
      below-normal priority; cleanup of orphaned files hourly. Files in `%LOCALAPPDATA%\ClipVault\waves`.
- [x] GUI `lanes::Waves` cache reloads changed files; strided max for zoomed-out views.
- [ ] Watch the CPU/disk impact while gaming. Each 1 h file is read once (~13 GB); consider throttling with
      `-readrate`, or pausing while a game is focused.

## 5. Smaller items
- [ ] "Music" track: maybe rename to "Music & media" and include browsers/VLC by default, so YouTube has
      its own slider. In separate-tracks mode, audio from apps that aren't in any list is currently only in
      Desktop/Full mix (the user asked about YouTube; awaiting their decision).
- [ ] Home page cards: content now stacks vertically (fixed); re-check at small window sizes.
- [x] Clip detail: lanes replace the mixer card; edits are saved per clip.
- [ ] Remove the self-test/mpv-log env hooks before release, or keep them documented.
- [ ] README: document install, settings and troubleshooting.

## Done (for reference)
- Core library (config with ~100 settings, SQLite library, OBS websocket client, OBS config writer,
  ffmpeg cut/export/probe/thumbnail, game detection, footage-hours retention, mic matching, hotkeys):
  87 unit tests, including real-ffmpeg and fake-OBS integration tests.
- Service: launches/supervises OBS, crash restarts, stalled-recording watchdog, 6-track audio routing,
  game-audio follows the focused game, apps on other devices added to the full mix, mic guard (fixes the
  A50 "new device ID on reconnect" problem), clip hotkey + overlay (your image) + toasts, tray icon,
  clip filing by game, stray-replay import, one-time tidy of the old clip folders (`migration.log`),
  retention, jobs (clip from range, trim, export to size), local IPC for the GUI.
- GUI: Home, 24/7 timeline with live playback, Clips library (games/categories/favourites/search,
  bulk actions), clip view (trim, export, categories, notes), Settings (12 sections).
- Multi-track playback mixing in libmpv (canonical-order `join` + per-track `af` gains, verified on real
  6-track OBS files).

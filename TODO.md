# ClipVault: remaining work

Status as of 2026-11-25. The Rust rewrite (core library, background service, GUI) builds
and runs on this PC. The service is managing OBS and recording 6 audio tracks. The items
below are what's left, in priority order.

## 1. Install on this PC (replace the old Python startup)
- [ ] Build release binaries (`cargo build --release`).
- [ ] Install to `%LOCALAPPDATA%\Programs\ClipVault\`: `clipvault.exe`, `clipvault-service.exe`,
      `libmpv-2.dll`, `ffmpeg.exe`/`ffprobe.exe` (copied from `C:\Program Files\ffmpeg-6.0-essentials_build\bin`),
      `assets\overlay_img.png`.
- [ ] Remove `main.py - Shortcut.lnk` from the Startup folder; create `ClipVault Service.lnk`
      (`clipvault-service.exe --install-shortcuts --desktop` does the Startup, Start Menu and Desktop links).
- [ ] Stop the dev-build service, start the installed one, and confirm it adopts the running OBS.
- [ ] Write `scripts/install.ps1` so this is repeatable.

## 2. 24/7 timeline & player (user feedback)
- [ ] The audio-tracks button (next to the speed menu) should show/hide **audio lanes under the timeline**
      (one lane per track: name, mute, volume, waveform). Currently it does nothing.
- [ ] **Scrubbing:** dragging on the timeline should move the playhead (fast keyframe seeks while dragging,
      exact seek on release). Pan by dragging the ruler, with Shift+wheel, or with right-drag. Make the
      playhead handle bigger.
- [ ] **Fullscreen:** the bottom controls are cut off. Lay the controls out with a bottom panel so the
      video takes only the remaining space (also fixes the clip view).
- [ ] **Marker panel overflow:** long marker names widen the side panel and push the whole UI off screen.
      Truncate labels and give the rename box a fixed width. Apply the same to the recording-file rows.
- [ ] **Activity lane:** label it "Activity", show app/game names inside wide blocks, and merge tiny gaps.
      (It shows which program was focused; "Claude" was the focused app.)

## 3. Non-destructive audio region edits
Verified in libmpv: per-track `volume=eval=frame:volume='...if(between(t,a,b),g,1)...'` mutes or boosts
exact time ranges with no audible artefacts.
- [ ] Core: `AudioEdit { track, start, end, gain }`; DB schema v2 adds `clips.audio_edits` (JSON).
- [ ] Core mixer: per-track gain expressions (base gain × region factors) for mpv `af` and ffmpeg export.
- [ ] GUI: drag on a lane to select a region, then Mute / Quieter / Louder / exact %, with an "all tracks"
      option. Existing regions are drawn on the lane; click one to edit or remove it.
- [ ] Creating a clip from the 24/7 range carries the edits and track volumes inside the range into the
      clip's metadata. **The file keeps every original track untouched** so edits can be undone later.
      The 24/7 recording itself is never modified.
- [ ] Clip view: same lanes and region editing, saved with the clip. Export bakes the edits into the shared file.
- [ ] Trim: shift the edits into the new clip's time.

## 4. Waveforms
- [ ] Service worker: one low-priority ffmpeg pass per file
      (`[0:a:i]aformat=mono … amerge, aeval=abs(..), aresample=200` → raw s16 on stdout) producing 10 peaks/s
      per track in `%LOCALAPPDATA%\ClipVault\waves\<hash>.cvw`. Newest files first, the live file
      incrementally (`-ss` from the last point) every ~60 s, clips too.
- [ ] GUI: load/refresh wave files (check mtime), draw them in the lanes (strided max for zoomed-out views).

## 5. Smaller items
- [ ] "Music" track: maybe rename to "Music & media" and include browsers/VLC by default, so YouTube has
      its own slider. In separate-tracks mode, audio from apps that aren't in any list is currently only in
      Desktop/Full mix (the user asked about YouTube; awaiting their decision).
- [ ] Home page cards: content now stacks vertically (fixed); re-check at small window sizes.
- [ ] Clip detail: replace the mixer card with lanes (same component as the 24/7 view).
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

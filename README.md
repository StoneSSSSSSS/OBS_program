# ClipVault

ClipVault records your Windows screen and app audio, saves highlights and exports an MP4 you can share. Recording, playback and export use the Windows media stack. You can run ClipVault without installing OBS, FFmpeg or mpv.

## Keep just the last two minutes

1. Open **Settings → 24/7 recording → Replay only**.
2. Choose **2 min**, then **Save**. Choose **Save & restart recording** when prompted. Other presets are 1, 5 and 10 minutes. **Custom** accepts 1–60 minutes.
3. Use **Home → Clip last 30 seconds** once the buffer has filled, or press your clip hotkey. Find the saved recording in **Clips**.

Replay only continually replaces temporary footage. Both clip hotkeys are limited to the selected window. Temporary files rotate about every ten seconds, so a file can overlap the edge of that window. Closing the recorder service clears unsaved replay footage. Your saved clips and earlier full-history recordings stay in the library.

## Select, edit and share a highlight

1. With **Full history** recording enabled, open **Home → Open latest recording** or choose a recorded section in **24/7 Recordings**.
2. Pause and click the timeline where the highlight starts. Choose **Set start** (I), seek to the finish and choose **Set end** (O). **Preview** plays the selection. **Save selected clip → Create** saves it. Choose **Open saved clip** in Jobs to edit it.
3. In the clip, choose **Separate tracks** to balance **Game**, **Voice chat**, **Music & media**, **Desktop** and **Microphone**. Expand **Apps** for individual recorded apps. Each browser appears as one app. **Full mix** plays the combined track on its own.
4. Drag across a waveform or choose **Edit audio range** to mute or change volume for part of a track. Use **Undo audio change** to reverse it. **Track names & visibility** customizes the lanes for this clip. **Settings → Player → Smooth audio edits** controls fades.
5. Preview the result. Choose **Export full quality** or set a limit and choose **Export to size limit**. Export uses the current mix and optional trim selection. The source clip retains its original audio tracks.

## Recording and storage

- Recordings go to `%USERPROFILE%\Videos\ClipVault\Recordings` by default. Clips have their own folder under `Videos\ClipVault`. Settings, the SQLite library, logs and thumbnails live under `%LOCALAPPDATA%\ClipVault`.
- The recorder uses Windows desktop capture, WASAPI sound capture and Media Foundation H.264/AAC codecs. It writes fragmented MP4 and can use a GPU encoder when available.
- **Full history** defaults to 30-minute files and 48 hours of retained footage. It also supports age, disk usage and free-space limits. Use the tray's exit action to stop the service completely.
- The default clip hotkey is **Ctrl + Shift + backtick**. Find saved clips in **Clips**, organized by game.
- Audio includes a full mix, microphone and a configurable number of simultaneous app tracks (eight by default). Apps can share a category. A browser is one app. Capture capabilities depend on Windows version and audio endpoints.
- Saving a clip retains its audio tracks. Export creates an MP4 with the selected mix baked into one audio track. The native tools support H.264/AAC MP4. Older six-track recordings in this format map to their original categories.

## Rust workspace

| Crate | Responsibility |
| --- | --- |
| core | Preferences, SQLite library, timeline math, non-destructive edit and mixer rules, GUI/service protocol |
| media | Windows capture and codecs, MP4 reader/writer, audio decoding, clipping, export, thumbnails and waveforms |
| service | Recorder lifecycle, tray and hotkeys, library maintenance, background cut and export jobs |
| gui | Windows desktop interface, timeline, clip library, player, mixer and settings |

For development, use a Windows Rust toolchain with a linker and Windows SDK. Run `cargo check --workspace` for a source check and `cargo test -p clipvault-core --lib` for the logic suite. Some media integration tests record the actual desktop or play sound. Use a dedicated test session for those. Set `CLIPVAULT_DATA_DIR` to an isolated directory and configure test media folders when exercising the service.

See [TODO.md](TODO.md) for the current rewrite and verification status.

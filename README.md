# ClipVault

ClipVault is a Windows screen recorder and clip editor. The background service records the screen and sound into a continuous timeline. You can keep a moment as a clip and make a shareable copy with your audio edits applied.

## Recording and storage

- Recordings go to `%USERPROFILE%\Videos\ClipVault\Recordings` by default. Clips have their own folder under `Videos\ClipVault`. Settings, the SQLite library, logs and thumbnails live under `%LOCALAPPDATA%\ClipVault`.
- The recorder uses Windows desktop capture, WASAPI sound capture and Media Foundation H.264/AAC codecs. It writes fragmented MP4 and can use a GPU encoder when available.
- Continuous recording defaults to 30-minute files and 48 hours of retained footage. Pausing continuous recording leaves a short rolling buffer for the clip hotkey. Use the tray's exit action to stop the service completely.
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

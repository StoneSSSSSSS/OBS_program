# Rust rewrite status

## Native backend

The recorder, MP4 reader/writer, lossless clip cutter, range export, thumbnail and waveform generation are implemented. The shared mixer supports category and app gains, old six-track recordings, non-destructive region edits and optional fades. Capture and export use Windows APIs.

The previous session reported a 1728 × 1080 recording at 60 fps and a roughly 28-second hotkey clip on its test laptop. This is machine-specific evidence. Device coverage, long recording sessions, low disk space, display changes and audio reconnects still need hands-on verification.

## Desktop editing milestone

- Connect the shared desktop player to the native decoder. Verify playback, pause, seek, synchronization and reopening a clip.
- Use the same time-dependent audio track assignments and mixer for preview and export. Verify a legacy six-track file and a recording where the same slot changes apps.
- Make start/end selection easy to discover and keep the endpoints ordered. Verify a cut across recording segments.
- Add expandable category/app lanes, audio-edit undo, clip zoom and pan, and per-clip track names and visibility.
- Add a last-hour preview and a quick 30-second clip action to Home.
- Remove obsolete installer requirements and update native recording settings.
- Run a fresh usability review against the real window and record remaining limitations.

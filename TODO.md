# Rust rewrite status

## Working and verified on Windows

- [x] Record the screen and app audio, rotate recording files, generate live and saved waveforms, cut across files and export a selected range. A 2.5-second app tone survived the cut. The exported waveform went silent only during the selected mute.
- [x] Use one native player in the recording timeline and clip editor. Playback, pause, seek, restart and a growing MP4 passed. A 25 fps test video with numbered frames measured at most 10 ms between its displayed-frame timestamp and the WASAPI playback clock at the sampled points.
- [x] Match preview and export using group and app gains, old six-track recordings, a reused app slot, optional fades and non-destructive audio edits. Clip display names and hidden lanes are saved per clip.
- [x] Select an ordered range, save it, reopen it, seek, undo an audio change and export through the desktop UI. The second hands-on review produced a 2.517-second MP4 of 163,628 bytes from its saved clip.
- [x] Show recent footage on Home, including an action to open the latest recording and a 30-second clip button. The editor supports zoom and pan on longer clips.
- [x] Keep short, temporary replay history. With nine encoded segments, the service saved 30.02 seconds for a 30-second request and capped a longer request at 60.01 seconds under a one-minute policy. Changing modes cleared temporary media and waveforms while preserving archived footage and saved clips. This test supplied the recorder flush timestamp.
- [x] Recover the previous file and SQLite metadata on tested failures during trim replacement. Reject output collisions and exercise rollback on a deferred database commit failure.
- [x] Pause archived waveform backfill at its next decode chunk while a classified game is in the foreground. Scheduling tests simulate focus and shutdown and verify that a 200 ms audio chunk waits for focus to clear and incurs at least 4 ms of throttle time before decoding.

## Next verification

- [ ] Repeat the desktop review on an executable containing the latest changes. The scored reviews were 73/100 and 92/100. Recheck a quick drag on a child waveform, keeping its slider in view, clear Undo feedback, long job titles, output size and instructions on empty recording screens.
- [ ] Run a sustained live replay-only session on a second Windows setup. Check hotkey capture and rebinding, headset and display reconnects, disk pressure, and waveform backfill while entering and leaving a game.
- [ ] Measure backfill on long, multitrack archives alongside recording and playback. Check CPU and disk usage, actual pause and resume latency, and recorder dropped frames. The scheduling tests do not measure hardware or OS behavior.
- [ ] Test interruption by process termination or power loss during a trim. The file and database rollback tests exercise ordinary reported errors.

The previous handoff reported 1728 × 1080 at 60 fps and a roughly 28-second hotkey clip. Those figures came from that machine. See [README.md](README.md) for the current workflow.

# Rust rewrite status

## Working and verified on Windows

- [x] Record the screen and app audio, rotate recording files, generate live and saved waveforms, cut across files and export a selected range. A 2.5-second app tone survived the cut. The exported waveform went silent only during the selected mute.
- [x] Use one native player in the recording timeline and clip editor. Playback, pause, seek, restart and a growing MP4 passed. A 25 fps test video with numbered frames measured at most 10 ms between its displayed-frame timestamp and the WASAPI playback clock at the sampled points.
- [x] Match preview and export using group and app gains, a reused app slot, optional fades and non-destructive edits. A legacy six-track fixture produced matching preview and export tone levels at the sampled points. Clip display names and hidden lanes are saved per clip.
- [x] Select an ordered range, save it, reopen it, seek, undo an audio change and export through the desktop UI. A hands-on review saved a 16.277-second clip and exported a matching-duration MP4 of 978,904 bytes.
- [x] Show recent footage on Home, including an action to open the latest recording and a 30-second clip button. The editor supports zoom and pan on longer clips.
- [x] Record a live two-minute replay buffer. The Windows recorder wrote its first 10.01-second segment, expired that segment and its waveform at 130.864 seconds, saved 30.016 seconds for a 30-second request and capped a 300-second request at 120.021 seconds. The microphone was disabled. Shutdown cleared temporary footage and files while preserving archived recordings and saved clips through fixture teardown.
- [x] Exercise a full archive day with 48 native fragmented half-hour segments. The accelerated test cut 20.011 seconds across midnight, cut the final 30 seconds, evicted the oldest segment at hour 24.5 and reopened the retained library. It used synthetic media with successive half-hour timestamps without waiting 24 hours.
- [x] Keep portable profiles separate from the installed Windows layout. Tests covered project-specific profiles and instance identity, rejecting outside-profile deletes and preserving media in project Trash. Retention removed an expired native recording while preserving a saved clip and export in the same folder. A separate Windows junction test verified detection of an alias from Kept recordings into Clips.
- [x] Recover from controlled process termination during trim replacement. Isolated child processes stopped before and after the SQLite commit. Startup recovery restored the original before commit, kept the replacement after commit, and preserved conflicting files and newer edits. The tests also covered a migrated library, repeated recovery and rollback on a deferred database commit failure.
- [x] Keep long display titles independent of filenames. An IPC fixture preserved a 226 UTF-16-unit title at cut and a 182-unit title after renaming, with safe trim and export filename suffixes.
- [x] Pause archived waveform backfill at its next decode chunk while a classified game is in the foreground. Scheduling tests simulate focus and shutdown and verify that a 200 ms audio chunk waits for focus to clear and incurs at least 4 ms of throttle time before decoding.
- [x] Keep the player, waveforms, transport and inspector in separate regions during resizing. The previous build reproduced the overlap at intermediate widths. The fixed optimized executable passed 3,816 rendered frames across 18 views and UI scales of 100%, 125% and 150%. The sweep checks marked trim ranges, scrolled export actions and a dirty Settings footer. Saved screenshots include the 1094 × 671 case.
- [x] Complete an independent hands-on review of recent footage, range/preview, save/reopen, child-track mute/undo and size-limited export. All five tasks passed the review rubric. The reviewer verified the formerly failing widths during active Windows sizing and measured the 1 MB export at 770,655 bytes.
- [x] Scope the completed export shown in the inspector to the open clip. The complete export queue remains available in Jobs.

## Next verification

- [ ] Run a sustained live replay-only session on a second Windows setup. Check hotkey capture and rebinding, headset and display reconnects, disk pressure, and waveform backfill while entering and leaving a game.
- [ ] Measure backfill on long, multitrack archives alongside recording and playback. Check CPU and disk usage, actual pause and resume latency, and recorder dropped frames. The scheduling tests do not measure hardware or OS behavior.
- [ ] Verify trim recovery after a real power interruption on a disposable Windows setup.

The previous handoff reported 1728 × 1080 at 60 fps and a roughly 28-second hotkey clip. Those figures came from that machine. See [README.md](README.md) for the current workflow.

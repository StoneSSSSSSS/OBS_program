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

- [x] Verify ten-minute Full history through the service. Six hundred seconds of ordinary footage remained after rotation, with kept recordings and saved clips intact after reopening.
- [x] Verify a native elapsed-hour rollover using timestamped synthetic video and AAC. Across-file cuts matched continuous-source packet bytes and timing, including audio crossing the file boundary.
- [x] Reproduce and repair overlapping archive cuts and incomplete-copy indexing. The old service doubled a 10-second overlap selection and interpreted a 96-byte partial as nearly 88 days. Both cases now have independent fixture verification.

- [x] In the Windows app, switch between short and wrapping category names at 900 logical pixels and verify identical heading, sort, thumbnail-size, and search rectangles. Click the frame buttons, move both trim handles, enlarge and restore the preview, and persist a custom player shortcut.
- [x] Compare 10,080 one-minute archive segments against the previous retention algorithm. All 10,070 expiry decisions matched. Three release-mode runs of the in-memory planner took 0.5–0.6 ms, compared with 7.97–8.11 s previously. These timings exclude file operations.
- [x] Benchmark the native player on the same 25 fps synthetic file. Each paired five-second playing interval presented 125 frames. The paused static player eliminated all 24 periodic worker wakeups seen in the prior five-second samples. Active CPU varied and resident memory during playback stayed near 32 MiB.

- [x] Follow an active native Windows Graphics Capture recording into its next file in the visible player. The isolated test recorded an animated window with silent audio. Playback advanced as fragments were committed and followed rollover within 550 ms of the written edge in the observed run. Loading a new file now invalidates the previous file's playback state.
- [x] Enlarge the recording or clip preview while retaining both visible trim handles and time labels in a compact timeline. The Windows run checked the painted grip bounds and restoring the full timeline.

- [x] Keep the live row's Play button, LIVE badge, file size and lock readable. Keep Export and Marker fully visible or scrollable after resizing the preview. Native Windows checks passed at 100%, 125% and 150% UI scale.
- [x] Save and reopen two bindings for the same player action. In native UI tests, typing the primary key into recording search or the alternate into a clip title preserved the paused frame. Each key jumped to the end outside its text field.

- [x] Run ten-minute Full history against real, newly captured footage for 626 seconds. The owned-window WGC run expired two ordinary segments and kept 600.000 seconds of finalized footage plus the current eight-second durable tail. An eight-second recording marked Keep and a saved four-second clip kept their original bytes. This opt-in soak uses eight-second test segments. The ordinary ten-minute configuration uses sixty-second segments.

## Desktop Editor verification

- [x] Build an editable, multi-source timeline with native audio/video previews and exports, portable project files and offline searchable help. See [Editor features](docs/editor-features.md).
- [x] Verify full-HD landscape and portrait delivery, audio fades, subtitle visibility at cue boundaries, rotation and mirroring with actual native renders.
- [x] Exercise the Workbench, expanded preview and Wiki from 360 to 1300 logical points. The initial 21-screen sweep passed at 100%, 125% and 150% UI scale. The expanded 23-screen sweep also passed at each scale (1,500 rendered frames per scale), including a complete video lane and Inspector actions at short window heights. The 24-screen sweep also checked the selected audio Media Bin with an import banner and passed 1,512 rendered frames per scale.
- [ ] Finish independent editing challenges and document the outcome of each retest. [Review briefs and rubric](docs/editor-challenges.md)

## Next verification

- [ ] Run a sustained live replay-only session on a second Windows setup. Check hotkey capture and rebinding, headset and display reconnects, disk pressure, and waveform backfill while entering and leaving a game.
- [x] Measure waveform backfill alongside native capture and playback on this Windows machine. A 30-minute, three-track recording produced 18,000 peaks on each track. During eight seconds of owned-window recording, backfill added about 9.6 percentage points of one logical CPU core and six megabytes of service reads. Both recording windows reported zero dropped frames. During muted playback, the player delivered 199 frames without backfill and 200 with it. Simulated focus changes paused work within 1.5 ms and resumed it within 229 ms. The archive was warm in the Windows cache. Reproduce with `scripts/check-backfill.ps1`.
- [ ] Verify trim recovery after a real power interruption on a disposable Windows setup.

The previous handoff reported 1728 × 1080 at 60 fps and a roughly 28-second hotkey clip. Those figures came from that machine. See [README.md](README.md) for the current workflow.

# ClipVault

ClipVault records your Windows screen and app audio, saves highlights and exports an MP4 you can share. Recording, playback and export use the Windows media stack. You can run ClipVault without installing OBS, FFmpeg or mpv.

## Open ClipVault

Extract the whole Windows app folder somewhere you can write and open **clipvault.exe**. Keep it beside `clipvault-service.exe`, the bundled Microsoft C++ runtime, and the assets folder. The window starts the background recorder. Closing the window lets the recorder continue. Use **Quit ClipVault service** in the tray when you want to stop capturing.

## Keep ten minutes on the 24/7 timeline

1. Open **Settings → Recording & storage** and choose **Full history**.
2. Under **Keep this much footage**, choose **10 min** or enter a custom number of minutes. The minimum is **1 min**.
3. Click **Save**. If the recorder needs a new file length, choose **Save & restart recording**.

Ten-minute history targets one-minute files. The recorder rotates at the next video keyframe. Cleanup removes whole files, so a little extra footage may remain. Saved clips and recordings marked **Keep** are protected. Time the PC is off does not count.

## Keep just the last two minutes

1. Open **Settings → Recording & storage → Replay only**.
2. Choose **2 min**, then **Save**. Choose **Save & restart recording** when prompted. Other presets are 1, 5 and 10 minutes. **Custom** accepts 1–60 minutes.
3. Use **Home → Clip last 30 seconds** once the buffer has filled, or press your clip hotkey. Find the saved recording in **Clips**.

Replay only continually replaces temporary footage. Both clip hotkeys are limited to the selected window. Temporary files rotate about every ten seconds, so a file can overlap the edge of that window. Closing the recorder service clears unsaved replay footage. Your saved clips and earlier full-history recordings stay in the library.

## Select, edit and share a highlight

1. With **Full history** recording enabled, open **Home → Open latest recording** or choose a recorded section in **24/7 Recordings**.
2. Drag the divider under the preview, or click **Larger preview** to watch. The frame buttons beside Play step backward and forward. **Go live** follows the newest playable footage. **Jump to latest** also works when recording has stopped.
3. Pause and click the timeline where the highlight starts. Choose **Set start** (I), seek to the finish and choose **Set end** (O). Drag either timeline handle to fine-tune it. **Preview** plays the selection. **Save selected clip → Create** saves it. Choose **Open saved clip** in Jobs to edit it.
4. In the clip, choose **Separate tracks** to balance **Game**, **Voice chat**, **Music & media**, **Desktop** and **Microphone**. Expand **Apps** for individual recorded apps. Each browser appears as one app. **Full mix** plays the combined track on its own.
5. Drag across a waveform or choose **Edit audio range** to mute or change volume for part of a track. Use **Undo audio change** to reverse it. **Track names & visibility** customizes the lanes for this clip. **Settings → Player → Smooth audio edits** controls fades.
6. Preview the result. Choose **Export full quality** or set a limit and choose **Export to size limit**. Export uses the current mix and optional trim selection. The source clip retains its original audio tracks.

Change player shortcuts under **Settings → Hotkeys → In the player**. The default previous and next frame keys are comma and period. **End** goes to the latest footage or the final frame of a saved clip.

In a narrow window, use **Timeline** to play or select footage in **24/7 Recordings**, then **Files & jobs** to see recording files or open a finished clip. In a clip, use **Player** for preview, seeking, and audio lanes. Switch to **Trim & export** for the clip details and export controls. Widen the window to see both views at once.

Long clip titles stay in the library, including punctuation and emoji. ClipVault creates a Windows-safe filename and shows the finished filename in Jobs. Use **Open folder** to highlight that file in Explorer.

## Recording and storage

- The portable app keeps recordings, clips, exports, settings, thumbnails, logs and its SQLite library in `.clipvault-local\profile`. In a source checkout that folder is at the project root and Git ignores it. Choose **Settings → About → Open data folder** to find it. The portable app starts when opened and keeps discarded clips in its own trash folder when that option is enabled.
- The optional per-user installation keeps its existing Windows layout. Settings and the library live in `%LOCALAPPDATA%\ClipVault`. Default recording and clip folders are under `%USERPROFILE%\Videos\ClipVault`.
- The recorder uses Windows desktop capture, WASAPI sound capture and Media Foundation H.264/AAC codecs. It writes fragmented MP4 and can use a GPU encoder when available.
- New **Full history** profiles use one-hour files and retain 48 hours of recorded time. Existing file-length preferences are preserved. Mark one start and one end on the 24/7 timeline to save a single clip across consecutive files. It also supports age, disk usage and free-space limits. Use the tray's exit action to stop the service completely.
- Keep Recordings separate from Clips, Exports and internal storage. Exports can use their own subfolder under Clips. The service leaves saved clips, exports and legacy media alone during automatic recording cleanup.
- The default clip hotkey is **Ctrl + Shift + backtick**. Find saved clips in **Clips**, organized by game.
- Audio includes a full mix, microphone and a configurable number of simultaneous app tracks (eight by default). Apps can share a category. A browser is one app. Capture capabilities depend on Windows version and audio endpoints.
- Saving a clip retains its audio tracks. Export creates an MP4 with the selected mix baked into one audio track. The native tools support H.264/AAC MP4. Older six-track recordings in this format map to their original categories.
- If the service is interrupted during an in-place trim, it checks the pending operation at startup against the library and the files on disk. An uncommitted trim can restore the original, and a committed trim keeps the new clip. If the destination has changed, ClipVault leaves the files in place for inspection.

## Rust workspace

| Crate | Responsibility |
| --- | --- |
| core | Preferences, SQLite library, timeline math, non-destructive edit and mixer rules, GUI/service protocol |
| media | Windows capture and codecs, MP4 reader/writer, audio decoding, clipping, export, thumbnails and waveforms |
| service | Recorder lifecycle, tray and hotkeys, library maintenance, background cut and export jobs |
| gui | Windows desktop interface, timeline, clip library, player, mixer and settings |

For development, use a Windows Rust toolchain with a linker and Windows SDK. Run `cargo check --workspace` for a source check and `cargo test -p clipvault-core --lib` for the logic suite. Some media integration tests record the actual desktop or play sound. Use a dedicated test session for those. Set `CLIPVAULT_DATA_DIR` to an isolated directory and configure test media folders when exercising the service.

To build the two Windows executables from source, run `cargo build --release --locked` at the workspace root. Open `target\release\clipvault.exe` to run that build. For a per-user installation with shortcuts, run `scripts\install.ps1` from PowerShell after the release build finishes.

See [TODO.md](TODO.md) for the current rewrite and verification status.

### Check resizing after a UI change

The Windows layout workflow runs the editor, page layout and pointer-input regression tests when the rewrite branch changes. Locally, `cargo test --locked -p clipvault-gui --bin clipvault views::` exercises retained layouts as the window shrinks and grows.

For a release check on a Windows desktop, use [scripts/check-resize.ps1](scripts/check-resize.ps1). Pass `-Gui` the candidate executable, `-FixtureService` the test executable from `cargo test --locked -p clipvault-service --no-run`, and `-Media` a synthetic H.264/AAC MP4 with at least 30 seconds of footage. Run with `-Scale 1`, `1.25`, and `1.5`. Each run opens a disposable library, measures the rendered text during native resizing, and leaves intermediate screenshots and a pass/fail report under `.clipvault-local\qa`.

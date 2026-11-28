# Independent Editor challenges

Each tester gets one brief, a fresh profile and a private copy of the synthetic inputs. Give the tester the desired result, the application, the Wiki and their output folder. Keep implementation details, other reviews and expected scores private. Each test ends with a playable MP4 and a saved project unless its brief explicitly tests a failure. Use new testers after a fix.

| Brief | Assignment | What to verify independently |
| --- | --- | --- |
| Sports highlight | Assemble the final play from one recording and the replay from a second. Remove the lead-ins and switch to the replay on the chosen action. | Two distinguishable clips appear in order. Inspect the splice one frame at a time. Check duration and original file hashes. |
| Narrated tutorial | Make a short screen tutorial using a voiceover and a quiet music bed. Leave a short music-only lead-in and keep speech easy to hear. | Check narration offset and levels in the intro and under speech. Compare the saved export with the preview at the same times. |
| Vertical montage | Make a portrait montage from a landscape video and two differently shaped stills. Start with a legible title card, then show all subjects in the requested order. | Decode the opener, each still and the closing frame. Verify portrait dimensions, title readability and each subject’s framing. |
| Client revision | Reopen a supplied rough cut. Move the closing shot to the beginning, make a duplicate, try a shorter version, undo it and deliver the revision under a new project name. | Reopen both projects independently. Check sequence, restored trim, source hashes and preservation of the original project. |
| Location sound | Make a silent source into a three-beat sequence using three occurrences of one sound effect. Make the middle beat quieter and keep the last beat out of the credits. | Measure the three peaks, their relative levels and the silent tail in the exported audio. |
| Frame-accurate correction | In a numbered-frame source, remove a single unwanted frame between two named frames. Use the keyboard to verify the neighboring frames and export. | Decode immediately before and after the cut. Check the chosen frame is absent and both neighbors are present. |
| Long assembly | Start from a long sequence with repeated shots. Zoom to a point near the end, change just that shot and return to an overview. | Verify the first and last shots, total length, and that the edit affects only the intended occurrence. Check controls remain usable on a narrow window. |
| Handoff and relink | Open a project whose media directory was renamed. Recover the missing input, save and deliver from the moved project folder. | Confirm the app names the missing file, accepts the correct replacement and reopens after moving the project with its assets. Check protected outputs and source hashes. |
| Accessible screening | Subtitle a short screening excerpt in two languages. Import a timed SRT, correct one cue, add a credit cue, and deliver both a captioned MP4 and a separate SRT. | Read the SRT back, compare cue boundaries, and inspect the rendered picture before, during and after each cue. Verify accented text and a multiline cue. |

## Prepare the review

Build the GUI test executable with `cargo test --locked -p clipvault-gui --bin clipvault --no-run`. Use the test executable printed by Cargo with `scripts/make-editor-inputs.ps1 -GuiTests <path>`. It creates two sports angles, a simulated screen tutorial, a source with 100 visibly numbered frames, landscape and portrait framing cards, a music bed, a short cue and offline spoken narration inside the project's ignored QA folder.

Run `scripts/start-editor-review.ps1 -Gui <clipvault.exe> -Brief <brief-name> -Inputs <generated-folder>` for each tester. The launcher opens a separate window and prints its process ID, input folder and output folder. Each tester receives private input copies with a SHA-256 manifest. For project-revision cases, provide a prepared editable project in that tester's private folder. For accessibility, include a short synthetic UTF-8 SRT with two timed cues. The GUI runs with its own profile and the recorder offline.

## Evidence and scoring

1. Record the exact build, input hashes, output paths, and the search phrases the tester tried in the Wiki. Use project-local synthetic data throughout.
2. Score the brief out of 100. Award up to 40 for the requested edit, 25 for verified export fidelity, 15 for project persistence and source preservation, 10 for understanding the controls, and 10 for finding help. Explain each deduction. Record a blocked step and its evidence without guessing what a later step would do.
3. Save screenshots for unexpected results and a reproducible action sequence. Separate a workflow observation from an idea for an enhancement.
4. After a fix, rerun the output verification and give the brief to a fresh tester. Keep the original result alongside the retest.

## Edge-case review

A separate reviewer can keep its earlier notes. Exercise an empty sequence, invalid trim, repeated source, missing or unsupported media, a destination collision, a cancelled export, a failed save, a long filename, a narrow window and keyboard focus in text fields. Check that a failed operation leaves existing files intact and can be retried. Report each case as passed, failed or untested with its evidence.

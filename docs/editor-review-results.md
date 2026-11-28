# Independent Editor reviews

Each first-time tester receives one editing brief, a separate running Windows app, private synthetic inputs and the in-app Wiki. The review awards 40 points for completing the edit, 25 for the published result, 15 for saving and preserving sources, 10 for controls and 10 for help. Keep each first result and its later retest separate. The local screenshots, editable projects and exported media live under the ignored `.clipvault-local/qa/editor-reviews` folder.

| Brief | First score | Evidence from the first delivery |
| --- | ---: | --- |
| Single-frame correction | 98 | Decoded all 99 numbered frames at 25 fps. The movie contains 000–046 and 048–099. The source hash matched. The frame-rate setting was hard to reach, and a new timestamp could appear over an older image. |
| Client revision | 95 | Moved, duplicated, trimmed, undid and reopened the title-card revision. The original hash matched and all three audio cues survived. Found a 20 ms picture at the 25 fps splice, an ambiguous active filename and an unsuccessful natural-language Wiki search. |
| Location sound | 94 | Measured the middle cue at 50.03% of the outer peaks and digital silence throughout the tail. The inspector separated placement from gain, and the help search missed “half volume.” |
| Portrait montage | 93 | Reopened and delivered a full-HD portrait title, video and two stills. All source hashes matched. Independent decoding found duplicate picture timestamps at two cuts. The app's separate frame reader also sheared portrait rows. |
| Media handoff | 96 | Restored both shots, reopened the portable folder, verified both sides of the 3.00 s splice and preserved an occupied export. Opening the project alone separately confirmed its media references were relative. An earlier error stayed on screen after a successful retry. |
| Long assembly | 97 | Shortened only occurrence 43 in a 44-shot assembly. All 51 independently decoded checks matched, including the last replay frame and the first frame of the final title. On a short window, scrolling to the timeline moved the preview out of view. |
| Accessible screening | 91 | Reopened the project and portable SRT, preserving the original accented and multiline text. Checked 22 published frames. Short captions covered the lower-right counter and remained visible one frame past their SRT end. |
| Narrated tutorial | 93 | The one-second music lead-in matched the muted-narration control. The bed measured 14.9 dB quieter under speech. A 10 ms remainder caused an error at the end of preview, and AAC padding extended the advertised exported duration. |
| Sports highlight | 96 | Cut from green wide frame 069 to amber replay frame 072 at 1.600 s. The two shots and all fixture hashes survived reopening. The reviewer found contradictory export feedback, a hidden frame-rate setting, and missing frame-step directions in the replay guide. |

## Fresh retests

| Brief | Retest | Independently verified result |
| --- | ---: | --- |
| Single-frame correction | 96 | A new tester removed 061, reopened the project and stepped 060 → 062 → 060. The finished delivery decoded to exactly 99 frames and all ten source hashes matched. The first export required a manual trim because the source's audio lasted 10 ms longer than its picture. The Inspector showed a stale edit indicator, and the Wiki lacked an ending check. |
| Client revision | 97 | A new tester reopened the named revision, verified both two-second titles and delivered a 14.50 s movie. The three embedded audio cues moved exactly four seconds. The rough cut and all ten sources matched their original hashes. Searching “move title to front” missed the article, and the guide used the video trim label for a title. |

The dedicated edge reviewer initially scored 87/100. A failed save, cancellation, destination collision, invalid trim and source recovery all preserved existing files. The review found cramped controls at 660 × 480, ambiguous duplicate-import feedback, a stale edit indicator and missing Japanese glyphs. The review runs keep the screenshots, projects, hashes and decoded output locally.

## Native verification

The dedicated Windows suites check rendered video dimensions and durations, source-frame matches, layered WAV and MP3 audio, trimmed native recordings, fades across splits, subtitle occupancy in a preview and its MP4, rotation, mirroring, cancellation and preservation of existing outputs. Separate resize sweeps cover the editor, expanded monitor and Wiki at three UI scales.

[Editing briefs](editor-challenges.md) · [Editor features](editor-features.md)

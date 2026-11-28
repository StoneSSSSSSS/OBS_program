//! Searchable, offline help for the multi-source Editor.
use crate::theme;
use eframe::egui::{self, RichText, Ui};

#[derive(Default)]
pub(crate) struct Wiki {
    query: String,
    topic: usize,
    section: Option<&'static str>,
    last_search: Option<(String, Option<&'static str>)>,
    results: Vec<(usize, usize)>,
    matching_titles: usize,
    show_related: bool,
}

struct Guide {
    section: &'static str,
    title: &'static str,
    terms: &'static str,
    intro: &'static str,
    steps: &'static [&'static str],
}

const GUIDES: &[Guide] = &[
    Guide { section: "Start here", title: "Make your first edit", terms: "begin beginner start import cut export tutorial quickstart",
        intro: "Turn two pieces of footage into a single video. Your source files stay available for future edits.",
        steps: &["Open Workbench and choose Import Media… to bring in two video files. You can also drop files onto the Editor.",
        "In the Media Bin, select the first video and choose Add to Timeline. Repeat for the next video in the order you want to watch them.",
        "Select a shot in the Timeline, set Source In and Source Out in the Inspector, then choose Apply Changes. Choose Play to check the cut.",
        "Use Project → Save Project As… to keep an editable .cvproj. Choose Export MP4… to make a video you can share."] },
    Guide { section: "Start here", title: "Import footage, stills and sound", terms: "supported formats file video mp4 png jpg jpeg wav mp3 music audio clips recording import media bin drag drop",
        intro: "The Media Bin holds the files you can use. You can use the same file several times with different trims.",
        steps: &["Choose Import Media… or drop files into the Workbench. Select one or more files in the picker.",
        "Start with an H.264 MP4 for video, a PNG or JPEG for a still, and WAV or MP3 for sound. Windows supplies the decoders for imported files.",
        "Select a video or still and add it to the Timeline. For audio, choose its role and place it at the playhead.",
        "Keep source files available after saving. To use footage from an active recording, save the desired range as a clip first and import that saved file."] },
    Guide { section: "Timeline", title: "Trim, split and remove a section", terms: "cut trim in out blade razor shorten ripple delete middle edit timeline",
        intro: "A trim changes each end of a shot. A split makes two independently editable pieces.",
        steps: &["Select the shot. In its Inspector set Source In and Source Out, then choose Apply Changes.",
        "To remove material in the middle, put the playhead at the first unwanted frame and choose Split.",
        "Select the piece on the right, put the playhead at the first frame you want to keep again and Split a second time.",
        "Select the unwanted middle piece and choose Remove. Later pictures shift to close the gap. A sound on a separate lane keeps its timeline start."] },
    Guide { section: "Timeline", title: "Remove exactly one frame", terms: "nudge frame by frame step advance previous next precise frames fps numbered counter frame counter",
        intro: "Match the project frame rate to the source when the cut must land on a particular source frame.",
        steps: &["Select your video in the Media Bin to see its source frame rate. Above Preview open the fps menu and choose Match selected source or 25 fps. One frame at 25 fps lasts 0.040 seconds.",
        "Select the shot, move to the first unwanted frame and choose Split. Use the frame controls to move forward exactly once.",
        "Split the following piece at the new position. Use the timeline item picker to select the tiny middle piece, then choose Remove.",
        "Step through the join and check both neighboring frames. After exporting, check the first and last pictures and the movie duration. Removing one frame from a 100-frame, 25 fps source leaves 99 frames lasting 3.960 seconds."] },
    Guide { section: "Timeline", title: "Reorder shots and revise a cut", terms: "move shot to beginning end front first last earlier later reorder rearrange duplicate copy undo redo client revision version save as",
        intro: "Each timeline occurrence has its own trims and volume, even when two shots use the same source.",
        steps: &["Use Project → Save Project As… to start a named revision of an existing edit.",
        "Select the closing shot or title and choose Move Earlier until it reaches the front. Move Later sends a shot toward the end. Duplicate inserts a second occurrence immediately after the selection.",
        "Drag the duplicate’s right trim handle left to try a shorter version. For an exact change, enter Duration for a title, color or still, or Source Out for a video. Press Enter to apply the typed value immediately, or click Apply Changes after leaving the field. Undo restores the previous length.",
        "Save the revision and reopen it to check the order before exporting."] },
    Guide { section: "Timeline", title: "Navigate a long sequence", terms: "zoom pan scroll horizontal scrub fit overview navigate playhead long timeline seek",
        intro: "The playhead is a position in the assembled video. A source start refers to time inside an imported file.",
        steps: &["Choose Fit to see the entire sequence. Open Select Item and choose the picture or sound to jump to its start. For the penultimate shot choose the second-to-last Video entry. Zoom in to inspect the cut.",
        "Choose Pan → Pan Left or Pan Right to move the visible time window. Click or drag on the ruler to seek within it.",
        "Use the frame controls to check a cut. Fit returns to the full sequence. On a narrow screen choose Timeline. Use View for Fit, Zoom or Pan when space is tight. Drag the divider beneath the preview to change its height when available."] },
    Guide { section: "Sound", title: "Keep or mute the original camera audio", terms: "audio embedded source footage camera microphone original recorded sound mute clip split reorder duplicate",
        intro: "The audio recorded in a video follows that shot when you move, copy or trim it.",
        steps: &["Select the video shot in the timeline. In the Inspector, choose Camera audio and leave Gain at 100% to keep the original level.",
        "To quiet just this occurrence, reduce Gain or set it to 0% to mute it. Choose Apply Changes and check the preview.",
        "Use the separate Voice, Music and Effects lanes for additional sound. Export MP4 includes the same gains and placements."] },
    Guide { section: "Sound", title: "Mix narration and music", terms: "voiceover voice over dialog dialogue gain quiet lower music soundtrack duck ducking audio level volume tutorial",
        intro: "Give narration, music and effects their own timeline items so you can change timing and volume independently.",
        steps: &["Add the picture first. Import your voiceover and music, then add each to its corresponding audio lane.",
        "Set the narration's timeline start for a short lead-in. Source start trims the beginning of the sound itself.",
        "Select the music, lower Gain in the Inspector and choose Apply Changes. Select the video to adjust its original sound separately.",
        "For ducking, split the music at the start and end of the narration and lower the middle piece. Sound splits use the exact playhead time. Picture splits use the nearest project frame."] },
    Guide { section: "Sound", title: "Place repeated sound effects", terms: "foley knocks footsteps sound design cue effects sfx beats audio snip repeat delayed delay timing offset silent silence tail ending credits half volume quiet quieter loud level gain percentage",
        intro: "Independent cues let one sound play at several points with a different level each time.",
        steps: &["Import the cue and select it in the Media Bin. Set the timeline Playhead field to the first entrance time. Select Effects and choose Add to Timeline, or drag the source onto the Effects lane.",
        "Duplicate the cue for each additional entrance. In the Inspector set Start on Timeline, then choose Apply Changes or press Enter.",
        "Set Gain to 50% for half volume on an individual occurrence. Apply Changes, then preview. Source Out trims the cue itself before the credits.",
        "For a silent tail, keep the picture running after the last effect. Select the video and choose Camera audio in the Inspector to mute its Gain. Hover over a short effect to see its full filename and timing, or zoom in for a longer label. Preview the final seconds. Audio extending beyond the last picture adds time on a black background."] },
    Guide { section: "Pictures", title: "Make a portrait or square montage", terms: "vertical portrait horizontal landscape aspect 9:16 16:9 square canvas resolution still photo montage social fit fill crop reframe zoom in focus subject left right top bottom framing letterbox letterboxing black bars pillarbox border",
        intro: "Choose the output shape first so you can check how landscape footage and portrait stills fit inside it.",
        steps: &["In Project Settings choose a portrait or square Canvas, then Apply Project Settings. The 1080 × 1920 portrait preset is full HD. Add pictures and videos in viewing order.",
        "Select a still, click its Duration value in the Inspector, press Ctrl+A, type the exact length in seconds (for example 1.50), then press Enter to apply. For a video, choose Picture in the Inspector. Under Framing, Fit keeps the whole source visible and can leave black bars. Fill crops the picture to cover the canvas.",
        "With Fill selected, adjust Horizontal Focus toward the left or right subject, or Vertical Focus toward the top or bottom. Choose Apply Changes. Drag the divider below Preview for a larger picture alongside the Inspector, or choose Expand Preview to check the whole composition. Each shot keeps its own framing.",
        "For an opener, choose + Title and select Beginning. Check text, framing and the final image in Preview.",
        "Export MP4… creates a file using the selected canvas and frame rate."] },
    Guide { section: "Pictures", title: "Rotate or mirror a picture", terms: "sideways rotate rotation upside down flip horizontal mirror orientation photo video",
        intro: "An orientation change applies to the selected occurrence of a video or still.",
        steps: &["Select the shot in the timeline. For a video, choose Picture in the Inspector, then find Rotation.",
        "Choose a clockwise angle. For a left-to-right reflection after rotation, select Mirror Horizontally.",
        "Choose Apply Changes. Use Expand Preview to inspect the result and Restore Preview to return to editing."] },
    Guide { section: "Sound", title: "Fade audio in or out", terms: "smooth volume soften ending ramp fade fades audio music click optional gain",
        intro: "A fade gradually changes volume while preserving your original recording.",
        steps: &["Select a sound occurrence or a video shot with audio. For a video, choose Camera audio in the Inspector. Set Fade in or Fade out in seconds.",
        "Choose Apply Changes and play across the boundary. Set a fade back to zero to remove it.",
        "Splitting a sound during a fade preserves the level on both sides of the cut."] },
    Guide { section: "Pictures", title: "Add timed captions or import subtitles", terms: "bilingual multilingual subtitle subtitles caption captions accessibility srt subrip transcript closed open burned burn cue boundaries text position bottom top background frame counter scoreboard legible keep visible",
        intro: "Time a subtitle to speech, burn it into the shared MP4, or save a separate SubRip file.",
        steps: &["Open Captions… and choose New at Playhead. Enter Caption text. Press Enter within the text box for a second language on a new line. Click a time, use Ctrl+A and type seconds or minutes:seconds, then press Enter. Choose Add Caption.",
        "The Captions lane shows each cue below the time ruler. Click a cue to jump to its start, or click its right edge to jump to the end. Double-click a cue to edit its text and times, then choose Save Caption.",
        "Use Import SRT… for existing subtitles. Caption position moves subtitles to the top or bottom of the picture so important detail stays visible. Export SRT… writes a separate subtitle file. Choose a fresh filename.",
        "Start is the first included time and End is the first time without that cue. Check both sides with Previous Frame and Next Frame. Export MP4… includes the captions in the picture."] },
    Guide { section: "Pictures", title: "Show closing credits on black after video", terms: "black after video last picture footage closing end credits captions subtitles blank screen extend hold tail",
        intro: "A caption can continue after the last video frame. The editor shows a black picture until the final caption ends.",
        steps: &["Seek to the end of the last picture. Open Captions… and choose New at Playhead. Enter the credit, set Start and End, then choose Add Caption.",
        "The Captions lane shows the cue below the time ruler. Click its start or right edge and step with comma or period to check the cut to black. The end time is the first instant without the cue.",
        "Choose Export MP4… for a movie with visible credits. For a separate subtitle file, open Captions… and choose Export SRT…."] },
    Guide { section: "Timeline", title: "Keyboard shortcuts and a larger preview", terms: "keys keyboard hotkeys shortcuts shortcut play pause step nudge enlarge expand restore save undo",
        intro: "Use these shortcuts when editing in the Workbench.",
        steps: &["Click the playback clock to open Go to time. Type seconds or minutes:seconds and press Enter. Space plays or pauses. Comma steps back one frame and period steps forward. S splits the selected item and Delete removes it.",
        "Ctrl+Z undoes and Ctrl+Y redoes. Ctrl+I imports media. Ctrl+O opens a project, Ctrl+S saves, and Ctrl+Shift+S saves a new version.",
        "Choose Expand Preview above the picture for a larger monitor. Use the scrub bar beneath it to seek. Choose Restore Preview for the editing layout."] },
    Guide { section: "Pictures", title: "Add a title card or solid color", terms: "slate title text credit opening chapter intertitle black blank color matte",
        intro: "Title cards and color frames occupy time in the picture sequence.",
        steps: &["Choose + Title, enter the text and duration, then choose Beginning for an opener or End for credits. Add to Timeline inserts the card in one step.",
        "Use + Color to add a plain frame. A short color frame can separate sections or give the soundtrack an intentional tail.",
        "Select the card to adjust its properties, then move it into position. Check it in the preview at the final canvas shape."] },
    Guide { section: "Files", title: "Save, reopen and move a project", terms: "save as open cvproj portable external file preserve share move handoff editable revision",
        intro: "A .cvproj stores edit decisions and links to your source files. An MP4 export contains the finished picture and sound.",
        steps: &["Use Project → Save Project As… for the first save or a new version. Save Project updates the current edit. For a revised MP4, choose Export MP4…. The file picker suggests the next available version, such as Game (2).mp4. If you choose an existing file, use Choose new filename… to retry.",
        "Copy the project and its media together. Keep each relative subfolder in the same place, including any sibling folders holding footage. Open the transferred project and check the Media Bin for Missing.",
        "Open Project… loads the sequence and all its edits. An offline source can be restored with Relink Media….",
        "To share the finished video for viewing, use Export MP4… and share that output."] },
    Guide { section: "Files", title: "Find missing media and relink it", terms: "missing offline not found broken moved renamed locate reconnect relink original sources file",
        intro: "Relinking updates references to a source while preserving trims, placement and gain.",
        steps: &["Open the project. Choose Relink in the missing-file notice beside the project name, or select a missing item in the Media Bin. Check its filename before choosing a replacement.",
        "Select the item, choose Relink Media… and locate its original file at the new path.",
        "Use the same kind of media with enough duration to cover the existing trims. Preview every occurrence, then save the project."] },
    Guide { section: "Files", title: "Export, cancel and find the result", terms: "render delivery job mp4 progress cancel overwrite existing location destination folder output share",
        intro: "Preview and export use the same shot order, source trims, timing and audio levels.",
        steps: &["Check the canvas, frame rate and end of the timeline. Choose Export MP4… and select a new output filename.",
        "Watch the progress indicator. If you cancel, wait for the job to finish cancelling before starting again.",
        "After completion, choose Open Output Folder to find the MP4. An occupied export name is refused and the existing file is preserved. Choose a fresh filename and retry. If the Editor names missing footage, choose Relink Media… and locate the source. Details opens long error messages."] },
    Guide { section: "Start here", title: "Cut a highlight and its replay", terms: "sports gameplay highlight replay two cameras angles archive recording boundary match cut",
        intro: "Cut the wide shot on the action, then show the same play from a second angle.",
        steps: &["In 24/7 Recordings, mark the play and save a selected clip. A range may cross adjacent recording files.",
        "Import the saved clip and a second camera angle into the Editor. Add the wide shot, then the replay.",
        "Trim each shot to the useful action. Step through the cut and adjust the replay's source start.",
        "Match the footage using the fps menu above Preview. Use Previous Frame and Next Frame, or comma and period, to check both sides of the cut. Balance each shot, save, then export."] },
];

fn search_terms(query: &str) -> impl Iterator<Item = &str> {
    query.split(|c: char| !c.is_alphanumeric()).filter(|word| !matches!(*word,
        "" | "a" | "an" | "the" | "how" | "do" | "does" | "i" | "my" | "it" | "to" | "can" | "t" |
        "what" | "where" | "is" | "with" | "for" | "of" | "and"))
}

fn has_term(text: &str, word: &str) -> bool {
    text.split(|c: char| !c.is_alphanumeric()).any(|token| token.starts_with(word))
}

fn rank(guide: &Guide, query: &str) -> Option<usize> {
    if query.is_empty() { return Some(0); }
    let title = guide.title.to_lowercase();
    let body = format!("{} {} {} {}", guide.section, guide.intro, guide.terms, guide.steps.join(" ")).to_lowercase();
    let mut score = 0;
    let mut terms = 0;
    let mut matched = 0;
    let query = query.to_lowercase();
    for word in search_terms(&query) {
        terms += 1;
        let weight = if has_term(&title, word) { 5 } else if has_term(guide.terms, word) { 3 }
            else if has_term(&body, word) { 1 } else { 0 };
        if weight > 0 { matched += 1; score += weight; }
    }
    if terms == 0 { return Some(0); }
    // A descriptive extra word should not hide a guide that matches the rest of a question.
    (matched * 3 >= terms * 2).then_some(score + if terms == matched { 20 } else { 0 })
}

pub(crate) fn show(ui: &mut Ui, state: &mut Wiki) {
    ui.label(theme::heading("Editor Wiki"));
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let width = (ui.available_width() - 55.0).max(30.0);
        let search = ui.add(egui::TextEdit::singleline(&mut state.query).hint_text("Search guides, tools or a task").desired_width(width));
        if ui.add_enabled(!state.query.is_empty(), egui::Button::new("Clear")).clicked() {
            state.query.clear();
            search.request_focus();
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.selectable_value(&mut state.section, None, "All topics");
        for name in ["Start here", "Timeline", "Sound", "Pictures", "Files"] {
            ui.selectable_value(&mut state.section, Some(name), name);
        }
    });
    if state.last_search.as_ref().is_none_or(|(query, section)| query != &state.query || *section != state.section) {
        let query = state.query.trim().to_lowercase();
        state.results = GUIDES.iter().enumerate()
            .filter(|(_, g)| state.section.is_none_or(|section| section == g.section))
            .filter_map(|(i, g)| rank(g, &query).map(|score| (i, score))).collect();
        let words = search_terms(&query).collect::<Vec<_>>();
        let title_hit = |i: usize| {
            let title = GUIDES[i].title.to_lowercase();
            words.len() >= 2 && words.iter().all(|word| has_term(&title, word))
        };
        state.results.sort_by_key(|&(i, score)| (!title_hit(i), std::cmp::Reverse(score), i));
        state.matching_titles = state.results.iter().take_while(|&&(i, _)| title_hit(i)).count();
        state.show_related = false;
        if let Some(&(first, _)) = state.results.first() { state.topic = first; }
        state.last_search = Some((state.query.clone(), state.section));
    }
    if state.results.is_empty() {
        ui.add_space(12.0);
        ui.label("No matching guides.");
        if ui.button("Show all topics").clicked() { state.query.clear(); state.section = None; }
        return;
    }
    let mut visible = state.results.as_slice();
    if state.matching_titles > 0 && state.matching_titles < visible.len() {
        ui.checkbox(&mut state.show_related, "Show related guides");
        if !state.show_related {
            visible = &visible[..state.matching_titles];
            if !visible.iter().any(|&(id, _)| id == state.topic) { state.topic = visible[0].0; }
        }
    }
    ui.separator();
    if ui.available_width() >= 650.0 {
        let (rect, _) = ui.allocate_exact_size(ui.available_size(), egui::Sense::hover());
        let split = rect.left() + (rect.width() * 0.32).clamp(200.0, 285.0);
        let list_rect = egui::Rect::from_min_max(rect.min, egui::pos2(split - 7.0, rect.bottom()));
        let article_rect = egui::Rect::from_min_max(egui::pos2(split + 7.0, rect.top()), rect.max);
        let mut list = ui.new_child(egui::UiBuilder::new().id_salt("wiki-sections").max_rect(list_rect));
        egui::ScrollArea::vertical().id_salt("wiki-list").show(&mut list, |ui| links(ui, &mut state.topic, visible, false));
        let mut reader = ui.new_child(egui::UiBuilder::new().id_salt("wiki-guide").max_rect(article_rect));
        egui::ScrollArea::vertical().id_salt(("wiki-article", state.topic)).show(&mut reader, |ui| article(ui, state.topic));
    } else {
        egui::ScrollArea::vertical().id_salt(("wiki-narrow", state.topic)).show(ui, |ui| {
            egui::ComboBox::from_id_salt("wiki-choose").width(ui.available_width().max(1.0)).truncate()
                .selected_text(GUIDES[state.topic].title).show_ui(ui, |ui| links(ui, &mut state.topic, visible, true));
            ui.add_space(8.0);
            article(ui, state.topic);
        });
    }
}

fn links(ui: &mut Ui, topic: &mut usize, results: &[(usize, usize)], popup: bool) {
    for &(i, _) in results {
        if ui.add(egui::Button::selectable(*topic == i, GUIDES[i].title).wrap()).clicked() {
            *topic = i;
            if popup { ui.close_kind(egui::UiKind::Popup); }
        }
    }
}

fn article(ui: &mut Ui, topic: usize) {
    let guide = &GUIDES[topic];
    ui.label(theme::small(guide.section));
    ui.add(egui::Label::new(RichText::new(guide.title).size(22.0).strong()).wrap());
    ui.add_space(5.0);
    ui.add(egui::Label::new(guide.intro).wrap());
    ui.add_space(7.0);
    for (i, step) in guide.steps.iter().enumerate() {
        ui.horizontal_top(|ui| {
            ui.label(theme::small(format!("{}.", i + 1)));
            ui.add(egui::Label::new(*step).wrap());
        });
        ui.add_space(6.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_finds_workflows_by_user_vocabulary() {
        for (query, expected) in [("black after video", "closing credits"), ("caption boundaries", "captions"), ("missing file", "relink"), ("frame counter", "one frame"), ("how do I nudge a frame", "one frame"),
            ("voiceover music", "narration"), ("camera audio", "camera audio"), ("portrait montage", "portrait"), ("penultimate shot", "long sequence"), ("cancel export", "Export"),
            ("move shot to beginning", "Reorder"), ("move title to front", "Reorder"), ("move closing chaptercard to front", "Reorder"), ("save as revision", "revise"), ("foley", "effects"), ("effects cue", "effects"), ("silent tail", "effects"), ("half volume", "effects"), ("letterbox", "portrait"), ("crop fill focus", "portrait")] {
            assert!(GUIDES.iter().any(|g| g.title.to_lowercase().contains(&expected.to_lowercase()) && rank(g, query).is_some()), "{query}");
        }
        assert!(GUIDES.iter().all(|g| rank(g, "unfindable0001").is_none()));
    }
}

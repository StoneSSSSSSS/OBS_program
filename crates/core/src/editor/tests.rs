use super::*;
use std::fs;

const S: u64 = TICKS_PER_SECOND;

fn info(kind: AssetKind, seconds: u64) -> AssetInfo {
    AssetInfo { kind, fps_milli: None, duration: seconds * S, width: 320, height: 192 }
}

#[test]
fn removes_one_frame_without_touching_neighbors_or_the_source() -> Result<()> {
    let mut p = Project::default();
    p.canvas.fps = 25;
    let a = p.import("count.mp4".into(), info(AssetKind::Video, 4))?;
    let original = p.append(a)?;
    let mut h = History::new(p);
    let isolated = h.edit(|p| p.split(original, 47 * S / 25))?;
    let right = h.edit(|p| p.split(isolated, 48 * S / 25))?;
    h.edit(|p| p.remove(isolated))?;
    assert_eq!(h.project.duration(), 99 * S / 25);
    assert_eq!(h.project.shots[0].range.end, 47 * S / 25);
    assert_eq!(h.project.shots[1].range.start, 48 * S / 25);
    assert_eq!(h.project.shot_start(right), Some(47 * S / 25));
    assert!(h.undo());
    assert_eq!(h.project.duration(), 4 * S);
    assert!(h.redo());
    assert_eq!(h.project.assets.len(), 1);
    Ok(())
}

#[test]
fn reversible_picture_and_audio_edits_keep_stable_ids() -> Result<()> {
    let mut p = Project::default();
    let a = p.import("scene.mp4".into(), info(AssetKind::Video, 8))?;
    let sound = p.import("cue.wav".into(), info(AssetKind::Audio, 4))?;
    let one = p.append(a)?;
    let two = p.duplicate(one)?;
    p.move_shot(two, 0)?;
    assert_eq!(p.shots[0].id, two);
    let cue = p.add_sound(sound, SoundRole::Effect, 3 * S)?;
    let duplicate = p.duplicate(cue)?;
    assert_eq!(p.sounds[1].at, 7 * S);
    let fade_point = p.split(duplicate, 8 * S)?;
    assert_eq!(p.sounds[2].range.start, S);
    assert_eq!(p.sounds[2].id, fade_point);
    let mut history = History::new(p);
    history.edit(|p| {p.sounds[1].gain = 0.2; Ok(())})?;
    let expected = history.project.clone();
    assert!(history.edit(|p| {p.shots[0].range.end = 99 * S; Ok(())}).is_err());
    assert_eq!(history.project, expected);
    assert!(history.edit(|p| p.split(one, 0)).is_err());
    assert!(history.undo());
    assert_eq!(history.project.sounds[1].gain, 1.0);
    history.edit(|p| p.move_shot(one, 0))?;
    assert!(!history.can_redo());
    Ok(())
}

#[test]
fn bounded_preview_matches_visual_trim_and_offset_audio() -> Result<()> {
    let mut p = Project::default();
    let a = p.import("scene.mp4".into(), info(AssetKind::Video, 16))?;
    let b = p.import("music.wav".into(), info(AssetKind::Audio, 24))?;
    p.append(a)?;
    p.shots[0].range = Span { start: 4 * S, end: 10 * S };
    p.title("Credits".into(), 2 * S)?;
    p.add_sound(b, SoundRole::Music, S)?;
    p.sounds[0].range = Span { start: 2 * S, end: 16 * S };
    p.sounds[0].gain = 0.24;
    let part = p.slice(Span { start: 5 * S, end: 10 * S })?;
    assert_eq!(part.duration(), 5 * S);
    assert_eq!(part.shots[0].range, Span { start: 9 * S, end: 10 * S });
    assert_eq!(part.shots[1].range.len(), 2 * S);
    assert_eq!(part.shots[2].source, Source::Color([0; 3]));
    assert_eq!(part.shots[2].range.len(), 2 * S);
    assert_eq!(part.sounds[0].at, 0);
    assert_eq!(part.sounds[0].range, Span { start: 6 * S, end: 11 * S });
    assert_eq!(part.sounds[0].gain, 0.24);
    part.validate()
}

#[test]
fn missing_media_can_be_relinked_without_resetting_edits() -> Result<()> {
    let mut p = Project::default();
    let a = p.import("source.mp4".into(), info(AssetKind::Video, 8))?;
    p.append(a)?;
    p.shots[0].range = Span { start: S, end: 4 * S };
    let old = p.clone();
    assert!(p.relink(a, "short.mp4".into(), info(AssetKind::Video, 3)).is_err());
    assert!(p.relink(a, "voice.wav".into(), info(AssetKind::Audio, 8)).is_err());
    assert_eq!(p, old);
    p.relink(a, "new-source.mp4".into(), info(AssetKind::Video, 4))?;
    assert!(p.assets[0].path.ends_with("new-source.mp4"));
    assert_eq!(p.shots, old.shots);
    Ok(())
}

#[test]
fn saved_edit_moves_with_inputs_and_leaves_existing_files_intact() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let kit = dir.path().join("client");
    let inputs = kit.join("Media");
    let edits = kit.join("Projects");
    fs::create_dir_all(&inputs)?;
    fs::create_dir_all(&edits)?;
    let source = inputs.join("Take 二.mp4");
    fs::write(&source, b"original footage")?;
    let mut p = Project::default();
    let a = p.import(source.clone(), info(AssetKind::Video, 6))?;
    p.append(a)?;
    let path = edits.join("Rough cut.cvproj");
    p.save(&path)?;
    let text = fs::read_to_string(&path)?;
    assert!(!text.contains("client"));
    let first = fs::read(&path)?;
    let mut invalid = p.clone();
    invalid.shots[0].range.end = 99 * S;
    assert!(invalid.save(&path).is_err());
    assert_eq!(fs::read(&path)?, first);
    p.name = "Delivery".into();
    p.save(&path)?;
    assert!(p.save(&source).is_err());
    let occupied = edits.join("foreign.cvproj");
    fs::write(&occupied, b"unrelated")?;
    assert!(p.save(&occupied).is_err());
    assert_eq!(fs::read(occupied)?, b"unrelated");
    let moved = dir.path().join("delivery");
    fs::rename(&kit, &moved)?;
    let restored = Project::load(&moved.join("Projects/Rough cut.cvproj"))?;
    assert_eq!(restored.name, "Delivery");
    assert_eq!(fs::read(&restored.assets[0].path)?, b"original footage");
    assert_eq!(restored.shots, p.shots);
    Ok(())
}

#[test]
fn split_and_preview_keep_audio_fades_at_the_same_source_samples() -> Result<()> {
    let mut p = Project::default();
    let sound = p.import("song.wav".into(), info(AssetKind::Audio, 12))?;
    let id = p.add_sound(sound, SoundRole::Music, S)?;
    let range = Span { start: 2*S, end: 10*S };
    let fade = AudioFade::for_range(range, 4*S, 2*S)?.unwrap();
    p.sounds[0].range = range;
    p.sounds[0].audio_fade = Some(fade);
    assert_eq!(fade.gain_at(4*S), 0.5);
    assert_eq!(fade.gain_at(9*S), 0.5);
    assert_eq!(fade.gain_at(6*S), 1.0);
    let right = p.split(id, 3*S)?;
    assert_eq!(p.sounds[1].id, right);
    assert_eq!(p.sounds[1].audio_fade, Some(fade));
    let crop = p.slice(Span {start: 3*S,end: 4*S})?;
    assert_eq!(crop.sounds[0].range.start, 4*S);
    assert_eq!(crop.sounds[0].audio_fade.unwrap().gain_at(crop.sounds[0].range.start), 0.5);
    assert!(AudioFade::for_range(range, 5*S, 4*S).is_err());
    assert!(AudioFade::for_range(range, 0, 0)?.is_none());
    Ok(())
}

#[test]
fn invalid_projects_fail_before_arithmetic_or_rendering() -> Result<()> {
    let mut p = Project::default();
    p.color([20, 40, 60], S)?;
    let mut broken = p.clone();
    broken.shots.push(p.shots[0].clone());
    assert!(broken.validate().is_err());
    p.shots[0].gain = f64::NAN;
    assert!(p.validate().is_err());
    p.shots[0].gain = 1.0;
    p.canvas.fps = 0;
    assert!(p.validate().is_err());
    p.canvas.fps = 30;
    p.shots[0].source = Source::Asset(356);
    assert!(p.validate().is_err());
    Ok(())
}

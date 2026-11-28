use anyhow::{Context, Result};
use cv_core::editor::{Canvas, Project, SoundRole, Span, TICKS_PER_SECOND as S};
use cv_media::{
    audioread,
    editor::{self, RenderControl},
    mf::h264dec::{DecodedFrame, VideoDecoder},
    mp4::{reader::{Mp4Reader, Sample}, Codec},
};
use std::{fs, path::{Path, PathBuf}};

const FRAME: u64 = S / 25;

fn reviewer() -> Result<PathBuf> {
    let local = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.clipvault-local").canonicalize()?;
    let fixtures = std::env::var_os("CLIPVAULT_EDITOR_REVIEW_FIXTURES")
        .map(PathBuf::from)
        .unwrap_or_else(|| local.join("qa/editor-reviews/client-review-979afd4c"))
        .canonicalize()?;
    anyhow::ensure!(fixtures.starts_with(local), "Cadence QA requires in-project reviewer fixtures");
    Ok(fixtures)
}

fn video_samples(reader: &Mp4Reader) -> Result<(u32, Vec<Sample>)> {
    let video = reader.video().context("The export has no video")?;
    let mut samples = reader.samples(video, 0, u64::MAX)?;
    samples.sort_by_key(|sample| sample.pts());
    Ok((reader.tracks[video].timescale, samples))
}

fn milliseconds(reader: &Mp4Reader, indexes: std::ops::Range<usize>) -> Result<Vec<i64>> {
    let (scale, samples) = video_samples(reader)?;
    Ok(samples[indexes].iter().map(|sample| sample.pts() * 1000 / i64::from(scale)).collect())
}

fn check_grid(reader: &Mp4Reader, project: &Project) -> Result<()> {
    let (scale, samples) = video_samples(reader)?;
    let count = (project.duration() * u64::from(project.canvas.fps)).div_ceil(S);
    assert_eq!(samples.len(), count as usize);
    for (i, sample) in samples.iter().enumerate() {
        assert_eq!(sample.pts() * i64::from(project.canvas.fps), i as i64 * i64::from(scale));
    }
    Ok(())
}

fn render(project: &Project, file: &Path) -> Result<Mp4Reader> {
    editor::render(project, file, &RenderControl::default())?;
    Mp4Reader::open(file)
}

fn add_cue(project: &mut Project, path: &Path, role: SoundRole, at: u64, duration: u64, gain: f64) -> Result<()> {
    let info = editor::probe(path)?;
    let id = project.import(path.into(), info)?;
    project.add_sound(id, role, at)?;
    let sound = project.sounds.last_mut().unwrap();
    sound.range.end = duration.min(info.duration);
    sound.gain = gain;
    Ok(())
}

fn rms(reader: &Mp4Reader, start: f64, end: f64) -> Result<f64> {
    let audio = *reader.audio().first().context("The export has no mixed audio track")?;
    let data = audioread::read_range(reader, audio, start, end)?;
    Ok((data.iter().map(|sample| f64::from(*sample).powi(2)).sum::<f64>() / data.len() as f64).sqrt())
}

#[test]
#[ignore = "uses the in-project independent reviewer’s exported MP4 and saved revision"]
fn restores_the_reviewers_frame_grid_with_three_timed_cues() -> Result<()> {
    let root = reviewer()?;
    let output = root.join("output/Client screening.mp4");
    let saved = root.join("revision/Client revision.cvproj");
    let old_bytes = fs::read(&output)?;
    let saved_bytes = fs::read(&saved)?;
    let work = tempfile::Builder::new().prefix("editor-frame-grid-").tempdir_in(
        root.parent().context("The review folder has no parent")?,
    )?;
    let original = Mp4Reader::open(&output)?;
    assert_eq!(milliseconds(&original, 249..253)?, [9960, 10000, 10020, 10060]);
    let mut project = Project::load(&saved)?;
    let replay = render(&project, &work.path().join("Rebuilt reviewer export.mp4"))?;
    assert_eq!(milliseconds(&replay, 249..253)?, [9960, 10000, 10040, 10080]);
    check_grid(&replay, &project)?;
    let (timescale, frames) = video_samples(&replay)?;
    assert_eq!(frames.last().unwrap().end() * 1000, 14_500 * u64::from(timescale));
    eprintln!("Reviewer frame starts at the cut: {:?} → {:?}", milliseconds(&original, 249..253)?, milliseconds(&replay, 249..253)?);

    let inputs = root.join("inputs");
    add_cue(&mut project, &inputs.join("Night drive music.wav"), SoundRole::Music, S / 2, 7 * S / 10, 0.22)?;
    add_cue(&mut project, &inputs.join("Voiceover.wav"), SoundRole::Voice, 2 * S, 3 * S / 2, 0.65)?;
    add_cue(&mut project, &inputs.join("Soft cue.wav"), SoundRole::Effect, 10 * S + S / 4, S / 2, 0.35)?;
    let cues = render(&project, &work.path().join("Rebuilt reviewer with cues.mp4"))?;
    check_grid(&cues, &project)?;
    for (label, quiet, playing) in [
        ("music", (0.13, 0.30), (0.60, 0.90)),
        ("voice", (1.40, 1.80), (2.05, 3.25)),
        ("effect", (10.09, 10.18), (10.32, 10.46)),
    ] {
        let quiet = rms(&cues, quiet.0, quiet.1)?;
        let playing = rms(&cues, playing.0, playing.1)?;
        eprintln!("{label}: before {quiet:.5}, active {playing:.5}");
        assert!(quiet < 0.004 && playing > 0.008, "{label} cue");
    }
    // Confirm the cues' static gain is preserved through cadence repair.
    let original_effect = rms(&cues, 10.32, 10.46)?;
    project.sounds.last_mut().unwrap().gain *= 2.0;
    let louder = render(&project, &work.path().join("Rebuilt reviewer louder effect.mp4"))?;
    let ratio = rms(&louder, 10.32, 10.46)? / original_effect;
    eprintln!("Effect gain doubled: measured RMS ratio {ratio:.4}");
    assert!((1.90..2.10).contains(&ratio));
    assert_eq!(fs::read(&output)?, old_bytes);
    assert_eq!(fs::read(&saved)?, saved_bytes);
    Ok(())
}

fn numbered_frames(reader: &Mp4Reader) -> Result<Vec<Option<u32>>> {
    let index = reader.video().context("Missing numbered video")?;
    let Codec::H264 { avcc } = &reader.tracks[index].codec else { anyhow::bail!("Expected H.264") };
    let mut decoder = VideoDecoder::new(avcc, None, false)?;
    let mut frames = Vec::new();
    for sample in reader.samples(index, 0, u64::MAX)? {
        let pts = sample.pts() * S as i64 / i64::from(reader.tracks[index].timescale);
        frames.extend(decoder.decode(&reader.read(&sample)?, pts)?);
    }
    frames.extend(decoder.drain()?);
    frames.sort_by_key(|frame| frame.pts_hns);
    Ok(frames.iter().map(number).collect())
}


fn number(frame: &DecodedFrame) -> Option<u32> {
    const SEGMENTS: [(usize, usize); 7] = [(2,0), (4,2), (4,6), (2,8), (0,6), (0,2), (2,4)];
    const DIGITS: [u8; 10] = [0x3f, 0x06, 0x5b, 0x4f, 0x66, 0x6d, 0x7d, 0x07, 0x7f, 0x6f];
    let mut value = 0;
    for digit in 0..3 {
        let mut mask = 0;
        for (index, (x, y)) in SEGMENTS.into_iter().enumerate() {
            let x = 70 + (digit * 7 + x) * 9 + 4;
            let y = 49 + y * 9 + 4;
            if frame.nv12[y * frame.width as usize + x] > 145 { mask |= 1 << index; }
        }
        value = value * 10 + DIGITS.iter().position(|candidate| *candidate == mask)? as u32;
    }
    Some(value)
}

#[test]
#[ignore = "decodes the actual rendered one-frame cut and three separate half-frame trims"]
fn retains_046_and_048_and_projects_multiple_half_frame_trims() -> Result<()> {
    let root = reviewer()?;
    let work = tempfile::Builder::new().prefix("editor-exact-cut-").tempdir_in(root.parent().unwrap())?;
    let path = root.join("inputs/Frame counter.mp4");
    let original = fs::read(&path)?;
    let mut project = Project::default();
    project.canvas = Canvas { width: 320, height: 192, fps: 25 };
    let source = project.import(path.clone(), editor::probe(&path)?)?;
    project.append(source)?;
    project.shots[0].range.end = 47 * FRAME;
    project.append(source)?;
    project.shots[1].range = Span { start: 48 * FRAME, end: 60 * FRAME };

    let result = render(&project, &work.path().join("Precisely removed 047.mp4"))?;
    check_grid(&result, &project)?;
    let frames = numbered_frames(&result)?;
    let numbers = [frames[45], frames[46], frames[47], frames[48]];
    eprintln!("Numbered decoded seam: {numbers:?}");
    assert_eq!(numbers, [Some(45), Some(46), Some(48), Some(49)]);

    project.shots[0].range = Span { start: S / 2, end: S + S / 2 };
    project.shots[1].range = Span { start: 2 * S + FRAME / 2, end: 2 * S + FRAME / 2 + 21 * FRAME };
    project.append(source)?;
    project.shots[2].range = Span { start: 5 * FRAME + FRAME / 2, end: 5 * FRAME + FRAME / 2 + 22 * FRAME };
    let result = render(&project, &work.path().join("Three half-frame trims.mp4"))?;
    check_grid(&result, &project)?;
    assert_eq!(fs::read(&path)?, original);
    Ok(())
}

fn independent_portrait_check(source: &Path, proof: &Path, reference: &Path) -> Result<()> {
    let decoder = std::env::var_os("CLIPVAULT_EDITOR_FFMPEG")
        .context("Set CLIPVAULT_EDITOR_FFMPEG to the independent FFmpeg executable")?;
    let project_local = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.clipvault-local").canonicalize()?;
    let proof = proof.canonicalize()?;
    anyhow::ensure!(proof.starts_with(project_local), "Independent decoder output must remain in the project");
    for (label, index, baseline) in [
        ("title", 18, "0.6"),
        ("court", 120, "4"),
        ("landscape-alignment", 330, "11"),
        ("portrait-alignment", 420, "14"),
    ] {
        let output = proof.join(format!("{label}.png"));
        let filter = format!("select=eq(n\\,{index}),scale=540:960");
        let decoded = std::process::Command::new(&decoder)
            .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-n", "-i"])
            .arg(source).args(["-vf", &filter, "-frames:v", "1"]).arg(&output)
            .output().context("Start the independent FFmpeg decoder")?;
        anyhow::ensure!(decoded.status.success(), "FFmpeg decode failed: {}", String::from_utf8_lossy(&decoded.stderr));
        let actual = image::open(&output)?.to_luma8();
        let old = image::open(reference.join(format!("ffmpeg-decoded-{baseline}.png")))?.to_luma8();
        assert_eq!(actual.dimensions(), old.dimensions());
        let mut shared = 0usize;
        let mut union = 0usize;
        let mut error = 0usize;
        for (&a, &b) in actual.as_raw().iter().zip(old.as_raw()) {
            shared += usize::from(a > 160 && b > 160);
            union += usize::from(a > 160 || b > 160);
            error += usize::from(a.abs_diff(b));
        }
        let overlap = shared as f64 / union as f64;
        let mae = error as f64 / actual.as_raw().len() as f64;
        eprintln!("Independent {label}: bright-mask overlap {overlap:.5}, mean luma error {mae:.5}");
        assert!(union > 100 && overlap > 0.85 && mae < 3.0, "{label} geometry differs from the independent source decode");
    }
    Ok(())
}


fn sample_luma(reader: &Mp4Reader, frame: usize, fps: u32, x: usize, y: usize) -> Result<u8> {
    let index = reader.video().context("Missing portrait video")?;
    let track = &reader.tracks[index];
    let Codec::H264 { avcc } = &track.codec else { anyhow::bail!("Missing H.264 header") };
    let time = frame as u64 * u64::from(track.timescale) / u64::from(fps);
    let wanted_hns = (time * S + u64::from(track.timescale / 2)) / u64::from(track.timescale);
    let key = reader.sync_at_or_before(index, time)?.context("No keyframe before the portrait cut")?;
    let mut decoder = VideoDecoder::new(avcc, None, false)?;
    let luma = |decoded: DecodedFrame| {
        (decoded.pts_hns.abs_diff(wanted_hns as i64) <= 1)
            .then(|| decoded.nv12[y * decoded.width as usize + x])
    };
    for packet in reader.samples(index, key.dts, time + u64::from(track.timescale / 4))? {
        let ticks = (packet.pts() * S as i64 + i64::from(track.timescale / 2)) / i64::from(track.timescale);
        if let Some(value) = decoder.decode(&reader.read(&packet)?, ticks)?.into_iter().find_map(luma) {
            return Ok(value);
        }
    }
    decoder.drain()?.into_iter().find_map(luma).context("Could not decode the portrait cut frame")
}

#[test]
#[ignore = "rebuilds the in-project portrait reviewer’s native duplicate-timestamp export"]
fn incoming_picture_wins_duplicate_timestamp_at_each_portrait_cut() -> Result<()> {
    let local = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.clipvault-local").canonicalize()?;
    let root = std::env::var_os("CLIPVAULT_EDITOR_PORTRAIT_REVIEW_FIXTURES").map(PathBuf::from)
        .unwrap_or_else(|| local.join("qa/editor-reviews/portrait-review-43b8c4bc")).canonicalize()?;
    anyhow::ensure!(root.starts_with(local), "Portrait QA requires in-project reviewer fixtures");
    let old = root.join("output/courtside-replay.mp4");
    let project_path = root.join("output/courtside-replay.cvproj");
    let old_bytes = fs::read(&old)?;
    let saved_bytes = fs::read(&project_path)?;
    let source = Mp4Reader::open(&old)?;
    let (scale, packets) = video_samples(&source)?;
    let duplicates: Vec<_> = packets.windows(2).filter(|pair| pair[0].pts() == pair[1].pts())
        .map(|pair| pair[0].pts() / i64::from(scale)).collect();
    assert_eq!(duplicates, [2, 13]);

    let project = Project::load(&project_path)?;
    let work = tempfile::Builder::new().prefix("editor-portrait-grid-").tempdir_in(root.parent().unwrap())?;
    let path = work.path().join("Rebuilt portrait cadence.mp4");
    let output = render(&project, &path)?;
    if let Some(proof) = std::env::var_os("CLIPVAULT_EDITOR_FRAME_PROOF") {
        independent_portrait_check(&path, Path::new(&proof), &root.join("output"))?;
    }
    check_grid(&output, &project)?;
    assert_eq!(video_samples(&output)?.1.len(), 450);
    assert_eq!(milliseconds(&output, 59..62)?, [1966, 2000, 2033]);
    assert_eq!(milliseconds(&output, 389..392)?, [12966, 13000, 13033]);
    let before_court = sample_luma(&output, 59, 30, 100, 800)?;
    let first_court = sample_luma(&output, 60, 30, 100, 800)?;
    let before_portrait = sample_luma(&output, 389, 30, 300, 360)?;
    let first_portrait = sample_luma(&output, 390, 30, 300, 360)?;
    eprintln!("Portrait cuts choose incoming picture: court Y {before_court} → {first_court}, still Y {before_portrait} → {first_portrait}");
    assert!(first_court > before_court + 35);
    assert!(first_portrait > before_portrait + 12);
    assert_eq!(fs::read(&old)?, old_bytes);
    assert_eq!(fs::read(&project_path)?, saved_bytes);
    Ok(())
}

#[test]
#[ignore = "uses the independent frame reviewer's synthetic numbered source and saved edit"]
fn default_import_removes_061_without_importing_the_aac_tail() -> Result<()> {
    let local = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.clipvault-local").canonicalize()?;
    let root = local.join("qa/editor-reviews/frame-rereview-50910c39");
    let source = root.join("inputs/Frame counter.mp4");
    let previous_output = root.join("tmp/first-export-with-extra-tail.mp4");
    let legacy_document = root.join("output/Frame counter without 061.cvproj");
    let source_bytes = fs::read(&source)?;
    let document_bytes = fs::read(&legacy_document)?;
    let work = tempfile::Builder::new().prefix("editor-picture-source-").tempdir_in(root.parent().unwrap())?;

    let original = Mp4Reader::open(&source)?;
    let original_video = original.video().context("Missing numbered source video")?;
    assert!(original.duration_secs() > 4.0);
    assert_eq!(original.end_secs(original_video), 4.0);
    let original_numbers = numbered_frames(&original)?;
    assert_eq!(original_numbers, (0..100).map(Some).collect::<Vec<_>>());

    let info = editor::probe(&source)?;
    assert_eq!(info.duration, 4 * S);
    let mut project = Project::default();
    project.canvas = Canvas { width: info.width, height: info.height, fps: 25 };
    let asset = project.import(source.clone(), info)?;
    let shot = project.append(asset)?;
    assert_eq!(project.shots[0].range.end, 4 * S);
    let removed = project.split(shot, 61 * FRAME)?;
    project.split(removed, 62 * FRAME)?;
    project.remove(removed)?;
    assert_eq!(project.duration(), 99 * FRAME);
    assert_eq!(project.shots[1].range.end, 4 * S);

    let edited = render(&project, &work.path().join("Imported frame 061 removed.mp4"))?;
    check_grid(&edited, &project)?;
    assert!((edited.duration_secs() - 3.960).abs() < 1e-8);
    let expected: Vec<_> = original_numbers.into_iter().filter(|number| *number != Some(61)).collect();
    let actual = numbered_frames(&edited)?;
    assert_eq!(actual, expected);
    eprintln!("Default imported end 4.000s. Removed 061, retained 060 and 062, and final 099 appears once in 99 frames");
    let previous = Mp4Reader::open(&previous_output)?;
    assert!(video_samples(&previous)?.1.len() > actual.len());
    assert!(previous.duration_secs() > edited.duration_secs());

    // A legacy project retains its authored source spans even though its
    // stored asset probe was longer. The reviewer had already set out to 4s.
    let mut legacy = Project::load(&legacy_document)?;
    assert_eq!(legacy.assets[0].info.duration, 4 * S + S / 100);
    assert_eq!(legacy.shots[1].range, project.shots[1].range);
    legacy.canvas = project.canvas;
    let rebuilt_legacy = render(&legacy, &work.path().join("Existing authored 4s out.mp4"))?;
    assert_eq!(numbered_frames(&rebuilt_legacy)?, actual);
    legacy.shots[1].range.end = legacy.assets[0].info.duration;
    let old_authored = work.path().join("Saved legacy explicit tail.cvproj");
    legacy.save(&old_authored)?;
    assert_eq!(Project::load(&old_authored)?.shots[1].range.end, 4 * S + S / 100);

    let sound = work.path().join("Intentional 4.010s audio.wav");
    let len = 192_480u32 * 2;
    let mut wave = b"RIFF".to_vec();
    wave.extend((36 + len).to_le_bytes());
    wave.extend(b"WAVEfmt ");
    wave.extend(16u32.to_le_bytes());
    wave.extend(1u16.to_le_bytes());
    wave.extend(1u16.to_le_bytes());
    wave.extend(48_000u32.to_le_bytes());
    wave.extend(96_000u32.to_le_bytes());
    wave.extend(2u16.to_le_bytes());
    wave.extend(16u16.to_le_bytes());
    wave.extend(b"data");
    wave.extend(len.to_le_bytes());
    wave.resize(wave.len() + len as usize, 0);
    fs::write(&sound, wave)?;
    assert_eq!(editor::probe(&sound)?.duration, 4 * S + S / 100);
    assert_eq!(fs::read(&source)?, source_bytes);
    assert_eq!(fs::read(&legacy_document)?, document_bytes);
    Ok(())
}

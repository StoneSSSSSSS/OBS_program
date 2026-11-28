use anyhow::{Context, Result};
use cv_core::editor::{AssetKind, Canvas, Project, SoundRole, Span, TICKS_PER_SECOND};
use cv_media::editor::{probe, render, thumbnail, RenderControl};
use cv_media::mp4::reader::Mp4Reader;
use std::{fs, path::{Path, PathBuf}};

const S: u64 = TICKS_PER_SECOND;

#[test]
#[ignore = "uses the local Windows H.264 encoder"]
fn full_hd_landscape_and_portrait_match_requested_delivery() -> Result<()> {
    let base = PathBuf::from(std::env::var_os("CLIPVAULT_DATA_DIR").context("Choose an isolated profile")?);
    fs::create_dir_all(&base)?;
    let work = tempfile::Builder::new().prefix("editor-hd-").tempdir_in(base)?;
    for (width, height) in [(1920, 1080), (1080, 1920)] {
        let mut project = Project::default();
        project.canvas = Canvas {width, height, fps: 25};
        project.color([65, 110, 85], S)?;
        let path = work.path().join(format!("{width}x{height}.mp4"));
        render(&project, &path, &RenderControl::default())?;
        let info = cv_media::probe::probe(&path)?;
        assert_eq!((info.width, info.height), (width, height));
        assert!((info.fps - 25.0).abs() < 0.01);
        assert!((info.duration - 1.0).abs() < 0.05);
    }
    Ok(())
}

fn fixture_root() -> Result<PathBuf> {
    let fixtures = PathBuf::from(std::env::var_os("CLIPVAULT_EDITOR_TEST_FIXTURES")
        .context("Set CLIPVAULT_EDITOR_TEST_FIXTURES to the generated editor QA kit")?);
    let local = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.clipvault-local").canonicalize()?;
    let fixtures = fixtures.canonicalize()?;
    anyhow::ensure!(fixtures.starts_with(local), "Editor QA must use an in-project fixture folder");
    Ok(fixtures)
}

fn add_shot(project: &mut Project, path: &Path, start: u64, end: u64) -> Result<()> {
    let asset = project.import(path.to_owned(), probe(path)?)?;
    project.append(asset)?;
    let shot = project.shots.last_mut().unwrap();
    shot.range = Span { start, end };
    shot.gain = 0.0;
    Ok(())
}

fn add_sound(project: &mut Project, path: &Path, role: SoundRole, at: u64, length: u64) -> Result<()> {
    let info = probe(path)?;
    assert_eq!(info.kind, AssetKind::Audio);
    let asset = project.import(path.to_owned(), info)?;
    project.add_sound(asset, role, at)?;
    let sound = project.sounds.last_mut().unwrap();
    sound.range.end = length.min(info.duration);
    sound.gain = 0.55;
    Ok(())
}

fn rms(pcm: &[f32]) -> f64 {
    (pcm.iter().map(|sample| f64::from(*sample).powi(2)).sum::<f64>() / pcm.len() as f64).sqrt()
}

fn counter_number(frame: &image::RgbaImage) -> Option<u32> {
    const CENTERS: [(u32, u32); 7] = [(2,0), (4,2), (4,6), (2,8), (0,6), (0,2), (2,4)];
    const DIGITS: [u8; 10] = [0x3f, 0x06, 0x5b, 0x4f, 0x66, 0x6d, 0x7d, 0x07, 0x7f, 0x6f];
    let mut value = 0;
    for digit in 0..3 {
        let mut mask = 0;
        for (index, (x, y)) in CENTERS.into_iter().enumerate() {
            let x = 10 + ((70 + (digit * 7 + x) * 9 + 4) * 180 + 96) / 192;
            let y = ((49 + y * 9 + 4) * 180 + 96) / 192;
            if frame.get_pixel(x, y)[0] > 145 { mask |= 1 << index; }
        }
        value = value * 10 + DIGITS.iter().position(|candidate| *candidate == mask)? as u32;
    }
    Some(value)
}

#[test]
#[ignore = "requires the local Windows editor QA kit"]
fn builds_and_publishes_a_native_edit() -> Result<()> {
    let root = fixture_root()?;
    let work = tempfile::Builder::new().prefix("native-edit-").tempdir_in(&root)?;
    let inputs = root.join("inputs");
    let counter = inputs.join("Frame counter.mp4");
    let portrait = inputs.join("Portrait alignment.png");
    let voice = inputs.join("Voiceover.wav");
    let original = fs::read(&counter)?;
    let mut project = Project::default();
    project.canvas = Canvas { width: 640, height: 360, fps: 30 };
    add_shot(&mut project, &counter, S / 2, S + S / 2)?;
    add_shot(&mut project, &portrait, 0, S)?;
    project.title("ClipVault\nNative editor".to_owned(), S)?;
    project.color([20, 85, 130], S)?;
    add_sound(&mut project, &inputs.join("Night drive music.wav"), SoundRole::Music, S / 2, 4 * S)?;
    add_sound(&mut project, &voice, SoundRole::Voice, S + S / 2, 2 * S)?;
    add_sound(&mut project, &inputs.join("Soft cue.wav"), SoundRole::Effect, 2 * S, S)?;

    let output = work.path().join("native-edit.mp4");
    let control = RenderControl::default();
    render(&project, &output, &control)?;
    assert_eq!(control.progress(), 1.0);
    assert_eq!(fs::read(&counter)?, original);
    let meta = cv_media::probe::probe(&output)?;
    eprintln!("export dimensions {:?}, fps {}, duration {:.5}, audio {:?}", (meta.width, meta.height), meta.fps, meta.duration, meta.audio);
    assert_eq!((meta.width, meta.height), (640, 360));
    assert!((meta.fps - 30.0).abs() < 0.1);
    assert!((meta.duration - 4.5).abs() < 0.08);
    let reader = Mp4Reader::open(&output)?;
    let tracks = reader.audio();
    assert_eq!(tracks.len(), 1);
    let before = rms(&cv_media::audioread::read_range(&reader, tracks[0], 0.1, 0.4)?);
    let music = rms(&cv_media::audioread::read_range(&reader, tracks[0], 0.7, 1.1)?);
    let layered = rms(&cv_media::audioread::read_range(&reader, tracks[0], 2.05, 2.19)?);
    eprintln!("audio RMS before={before:.5} music={music:.5} layered={layered:.5}");
    assert!(before < 0.005 && music > 0.01 && layered > 0.015);
    let preview = thumbnail(&project, S + S / 2, 320, 180)?;
    assert_eq!(preview.dimensions(), (320, 180));
    let mut output_project = Project::default();
    output_project.canvas = project.canvas;
    add_shot(&mut output_project, &output, 0, project.duration())?;
    let exported = thumbnail(&output_project, S + S / 2, 320, 180)?;
    let card = thumbnail(&project, 2 * S + S / 2, 320, 180)?;
    let matte = thumbnail(&project, 3 * S + S / 2, 320, 180)?;
    let tail = thumbnail(&project, 4 * S + S / 4, 320, 180)?;
    let side = preview.get_pixel(10, 90).0;
    let pad = exported.get_pixel(10, 90).0;
    let marker = |image: &image::RgbaImage| image.enumerate_pixels()
        .filter(|(x, y, _)| (145..175).contains(x) && (75..105).contains(y))
        .filter(|(_, _, pixel)| pixel[0] > 140).count();
    eprintln!("portrait side {side:?}, final {pad:?}, marker counts {} / {}", marker(&preview), marker(&exported));
    assert!(side[0] < 12 && pad[0] < 12);
    assert!(marker(&preview) > 15 && marker(&exported) > 15);
    let at = 3 * S / 10;
    let preview_counter = counter_number(&thumbnail(&project, at, 320, 180)?);
    let export_counter = counter_number(&thumbnail(&output_project, at, 320, 180)?);
    eprintln!("trimmed source frame at 0.3 s: preview {preview_counter:?}, export {export_counter:?}");
    assert!(preview_counter.is_some_and(|frame| frame.abs_diff(20) <= 1));
    assert!(export_counter.is_some_and(|frame| frame.abs_diff(20) <= 1));
    assert!(card.pixels().any(|rgb| rgb[0] > 140));
    let matte = matte.get_pixel(10, 90).0;
    assert!(matte[0].abs_diff(20) <= 8 && matte[1].abs_diff(85) <= 8 && matte[2].abs_diff(130) <= 8);
    assert!(tail.pixels().all(|rgb| rgb[0] < 12 && rgb[1] < 12 && rgb[2] < 12));

    let occupied = work.path().join("existing.mp4");
    fs::write(&occupied, b"existing output")?;
    assert!(render(&project, &occupied, &RenderControl::default()).is_err());
    assert_eq!(fs::read(&occupied)?, b"existing output");
    let cancelled = work.path().join("cancelled.mp4");
    let control = RenderControl::default();
    control.cancel();
    assert!(render(&project, &cancelled, &control).is_err());
    assert!(!cancelled.exists());
    assert_eq!(fs::read_dir(work.path())?.count(), 2);
    Ok(())
}


#[test]
#[ignore = "requires a completed in-project ClipVault recording"]
fn renders_the_clipvault_fragmented_full_mix() -> Result<()> {
    let root = fixture_root()?;
    let local = root.parent().context("Editor fixture directory has no parent")?;
    let input = PathBuf::from(std::env::var_os("CLIPVAULT_EDITOR_TEST_RECORDING")
        .context("Set CLIPVAULT_EDITOR_TEST_RECORDING to a completed ClipVault recording")?)
        .canonicalize()?;
    anyhow::ensure!(input.starts_with(local), "Recorder QA input must remain inside the project");
    let source = cv_media::probe::probe(&input)?;
    assert!(source.fragmented);
    let full = source.audio.iter().find(|track| track.role == cv_media::probe::TrackRole::Full)
        .context("Recorder fixture has no ClipVault full mix")?;
    let work = tempfile::Builder::new().prefix("native-recording-").tempdir_in(&root)?;
    let mut project = Project::default();
    project.canvas = Canvas { width: 360, height: 640, fps: 60 };
    let editor_source = probe(&input)?;
    assert!(editor_source.duration > 3 * S);
    let asset = project.import(input.clone(), editor_source)?;
    project.append(asset)?;
    project.shots[0].range = Span { start: S, end: 3 * S };
    let output = work.path().join("recording-excerpt.mp4");
    render(&project, &output, &RenderControl::default())?;
    let info = cv_media::probe::probe(&output)?;
    eprintln!("recorder export full track original {}; output {}x{} {:.2} fps", full.index, info.width, info.height, info.fps);
    assert_eq!((info.width, info.height), (360, 640));
    assert!((info.fps - 60.0).abs() < 0.1);
    assert!((info.duration - 2.0).abs() < 0.08);
    assert_eq!(info.audio.len(), 1);
    let original = Mp4Reader::open(&input)?;
    let rendered = Mp4Reader::open(&output)?;
    let reference = rms(&cv_media::audioread::read_range(&original, original.audio()[full.index], 1.1, 2.8)?);
    let excerpt = rms(&cv_media::audioread::read_range(&rendered, rendered.audio()[0], 0.1, 1.8)?);
    eprintln!("recorder full mix RMS {reference:.5}, export RMS {excerpt:.5}");
    assert!(reference > 0.005 && excerpt > reference * 0.65 && excerpt < reference * 1.35);
    let preview = thumbnail(&project, S / 2, 180, 320)?;
    assert_eq!(preview.dimensions(), (180, 320));
    assert!(preview.get_pixel(90, 7)[0] < 12);
    Ok(())
}


#[test]
#[ignore = "requires the local Windows editor QA kit"]
fn renders_distinct_windows_input_types() -> Result<()> {
    let root = fixture_root()?;
    let work = tempfile::Builder::new().prefix("native-kinds-").tempdir_in(&root)?;
    let inputs = root.join("inputs");
    let mut video = Project::default();
    video.canvas = Canvas { width: 640, height: 360, fps: 25 };
    add_shot(&mut video, &inputs.join("Frame counter.mp4"), 0, 3 * S)?;
    let mut video_twice = video.clone();
    add_shot(&mut video_twice, &inputs.join("Court replay.mp4"), S, 2 * S)?;
    let mut still = Project::default();
    still.canvas = video.canvas;
    add_shot(&mut still, &inputs.join("Portrait alignment.png"), 0, S)?;
    let mut mixed_visual = video.clone();
    add_shot(&mut mixed_visual, &inputs.join("Portrait alignment.png"), 0, S)?;
    let mut city = still.clone();
    city.shots.clear();
    city.title("Centered title".into(), S)?;
    let mut audio = Project::default();
    audio.canvas = video.canvas;
    add_sound(&mut audio, &inputs.join("Voiceover.wav"), SoundRole::Voice, S / 2, S)?;
    let mut mixed_audio = video.clone();
    add_sound(&mut mixed_audio, &inputs.join("Night drive music.wav"), SoundRole::Music, 0, 2 * S)?;
    add_sound(&mut mixed_audio, &inputs.join("Voiceover.wav"), SoundRole::Voice, S / 2, S)?;
    add_sound(&mut mixed_audio, &inputs.join("Soft cue.wav"), SoundRole::Effect, S, S)?;
    let cases = [
        ("video", video),
        ("two-videos", video_twice),
        ("still", still),
        ("title", city),
        ("mixed-pictures", mixed_visual),
        ("audio-only", audio),
        ("native-three-track-mix", mixed_audio),
    ];
    let mut errors = Vec::new();
    for (label, project) in cases {
        let output = work.path().join(format!("{label}.mp4"));
        match render(&project, &output, &RenderControl::default()) {
            Ok(()) => {
                let info = cv_media::probe::probe(&output)?;
                assert!((info.fps - 25.0).abs() < 0.1);
                eprintln!("{label}: {}x{} {:.2} fps, {:.3} seconds", info.width, info.height, info.fps, info.duration);
            }
            Err(err) => errors.push(format!("{label}: {err:#}")),
        }
    }
    anyhow::ensure!(errors.is_empty(), "{}", errors.join("\n"));
    Ok(())
}


#[test]
#[ignore = "requires the local Windows editor QA kit and Windows MP3 encoder"]
fn imports_mp3_and_renders_a_24_fps_audio_only_edit() -> Result<()> {
    use windows::{
        core::HSTRING,
        Media::{MediaProperties::{AudioEncodingQuality, MediaEncodingProfile}, Transcoding::MediaTranscoder},
        Storage::StorageFile,
        Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED},
    };
    let root = fixture_root()?;
    let work = tempfile::Builder::new().prefix("native-mp3-").tempdir_in(&root)?;
    let input = root.join("inputs").join("Soft cue.wav");
    let mp3 = work.path().join("Imported cue.mp3");
    fs::File::create_new(&mp3)?;
    let storage = |path: &Path| -> Result<StorageFile> {
        let absolute = path.to_string_lossy();
        let path = absolute.strip_prefix(r"\\?\").unwrap_or(&absolute);
        Ok(StorageFile::GetFileFromPathAsync(&HSTRING::from(path))?.get()?)
    };
    unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
    let generated = (|| -> Result<()> {
        let source = storage(&input)?;
        let destination = storage(&mp3)?;
        let encoder = MediaTranscoder::new()?;
        let profile = MediaEncodingProfile::CreateMp3(AudioEncodingQuality::High)?;
        let result = encoder.PrepareFileTranscodeAsync(&source, &destination, &profile)?.get()?;
        anyhow::ensure!(result.CanTranscode()?, "Windows MP3 encoder could not start");
        result.TranscodeAsync()?.get()?;
        Ok(())
    })();
    unsafe { RoUninitialize() };
    generated?;
    let mut project = Project::default();
    project.canvas = Canvas { width: 640, height: 640, fps: 24 };
    add_sound(&mut project, &mp3, SoundRole::Effect, S / 3, S)?;
    let output = work.path().join("mp3-mix.mp4");
    render(&project, &output, &RenderControl::default())?;
    let info = cv_media::probe::probe(&output)?;
    assert_eq!((info.width, info.height), (640, 640));
    assert!((info.fps - 24.0).abs() < 0.1);
    let reader = Mp4Reader::open(&output)?;
    let index = reader.audio()[0];
    let silence = rms(&cv_media::audioread::read_range(&reader, index, 0.05, 0.25)?);
    let cue = rms(&cv_media::audioread::read_range(&reader, index, 0.4, 0.54)?);
    eprintln!("24 fps MP3-only edit: before RMS {silence:.5}, cue RMS {cue:.5}");
    assert!(silence < 0.005 && cue > 0.01);
    Ok(())
}

#[test]
#[ignore = "runs and cancels a live native Windows encoder"]
fn cancels_an_in_flight_encode_without_leaving_a_file() -> Result<()> {
    use std::time::{Duration, Instant};
    let root = fixture_root()?;
    let work = tempfile::Builder::new().prefix("native-cancel-").tempdir_in(&root)?;
    let output = work.path().join("cancelled.mp4");
    let untouched = work.path().join("untouched.mp4");
    fs::write(&untouched, b"existing media")?;
    let mut project = Project::default();
    project.canvas = Canvas { width: 1280, height: 720, fps: 60 };
    project.color([10, 40, 110], 45 * S)?;
    let control = RenderControl::default();
    std::thread::scope(|scope| -> Result<()> {
        let job = scope.spawn(|| render(&project, &output, &control));
        let deadline = Instant::now() + Duration::from_secs(15);
        while control.progress() == 0.0 && !job.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let progress = control.progress();
        control.cancel();
        let error = job.join().expect("encode worker panicked").unwrap_err();
        eprintln!("cancelled an actual native encode at {:.1}%", progress * 100.0);
        assert!(progress > 0.0 && progress < 1.0);
        assert!(format!("{error:#}").contains("cancelled"));
        Ok(())
    })?;
    assert!(!output.exists());
    assert_eq!(fs::read(&untouched)?, b"existing media");
    assert_eq!(fs::read_dir(work.path())?.count(), 1);
    Ok(())
}

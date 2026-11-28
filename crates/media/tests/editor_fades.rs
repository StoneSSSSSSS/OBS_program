use anyhow::{Context, Result};
use cv_core::editor::{AudioFade, Canvas, Project, SoundRole, Span, TICKS_PER_SECOND};
use cv_media::{audioread, editor, mp4::reader::Mp4Reader};
use std::{fs, io::Write, path::{Path, PathBuf}};
use windows::{
    core::HSTRING,
    Media::{MediaProperties::{AudioEncodingQuality, MediaEncodingProfile}, Transcoding::MediaTranscoder},
    Storage::StorageFile,
    Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED},
};

const S: u64 = TICKS_PER_SECOND;

fn fixture_root() -> Result<PathBuf> {
    let local = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.clipvault-local").canonicalize()?;
    let root = PathBuf::from(std::env::var_os("CLIPVAULT_EDITOR_TEST_FIXTURES")
        .context("Set CLIPVAULT_EDITOR_TEST_FIXTURES to the in-project QA kit")?).canonicalize()?;
    anyhow::ensure!(root.starts_with(local), "Fade QA must use an in-project fixture folder");
    Ok(root)
}

fn tone(path: &Path) -> Result<()> {
    const RATE: u32 = 48_000;
    let mut file = std::io::BufWriter::new(fs::File::create_new(path)?);
    let samples = 3 * RATE;
    let bytes = samples * 2;
    file.write_all(b"RIFF")?;
    file.write_all(&(36 + bytes).to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&RATE.to_le_bytes())?;
    file.write_all(&(RATE * 2).to_le_bytes())?;
    file.write_all(&2u16.to_le_bytes())?;
    file.write_all(&16u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&bytes.to_le_bytes())?;
    for sample in 0..samples {
        let time = f64::from(sample) / f64::from(RATE);
        let value = (time * 420.0 * std::f64::consts::TAU).sin() * 16000.0;
        file.write_all(&(value.round() as i16).to_le_bytes())?;
    }
    file.flush()?;
    Ok(())
}

fn make_mp3(input: &Path, output: &Path) -> Result<()> {
    fs::File::create_new(output)?;
    let storage = |path: &Path| -> Result<StorageFile> {
        let wide = path.to_string_lossy();
        let normal = wide.strip_prefix(r"\\?\").unwrap_or(&wide);
        Ok(StorageFile::GetFileFromPathAsync(&HSTRING::from(normal))?.get()?)
    };
    unsafe { RoInitialize(RO_INIT_MULTITHREADED)? };
    let result = (|| -> Result<()> {
        let transcoder = MediaTranscoder::new()?;
        let profile = MediaEncodingProfile::CreateMp3(AudioEncodingQuality::High)?;
        let transcode = transcoder.PrepareFileTranscodeAsync(&storage(input)?, &storage(output)?, &profile)?.get()?;
        anyhow::ensure!(transcode.CanTranscode()?, "The Windows MP3 encoder is unavailable");
        transcode.TranscodeAsync()?.get()?;
        Ok(())
    })();
    unsafe { RoUninitialize() };
    result
}

fn export(project: &Project, output: &Path) -> Result<Mp4Reader> {
    editor::render(project, output, &editor::RenderControl::default())?;
    Mp4Reader::open(output)
}

fn rms(file: &Mp4Reader, start: f64, end: f64) -> Result<f64> {
    let index = *file.audio().first().context("The export has no audio")?;
    let pcm = audioread::read_range(file, index, start, end)?;
    Ok((pcm.iter().map(|sample| f64::from(*sample).powi(2)).sum::<f64>() / pcm.len() as f64).sqrt())
}

fn envelope(file: &Mp4Reader, offset: f64, label: &str) -> Result<()> {
    let before = rms(file, offset + 0.07, offset + 0.14)?;
    let entering = rms(file, offset + 0.27, offset + 0.36)?;
    let full = rms(file, offset + 0.82, offset + 1.18)?;
    let leaving = rms(file, offset + 1.64, offset + 1.73)?;
    let after = rms(file, offset + 1.86, offset + 1.93)?;
    eprintln!("{label} per-window RMS {before:.5} → {entering:.5} → {full:.5} → {leaving:.5} → {after:.5}");
    assert!(full > 0.05);
    assert!(before < entering * 0.6 && entering < full * 0.8);
    assert!(after < leaving * 0.6 && leaving < full * 0.8);
    Ok(())
}

#[test]
#[ignore = "compares real native fade envelopes for WAV, MP3, and video audio"]
fn verifies_linear_fades_and_source_time_anchor_after_slice_and_split() -> Result<()> {
    let root = fixture_root()?;
    let work = tempfile::Builder::new().prefix("native-fades-").tempdir_in(root)?;
    let wave = work.path().join("Voice sample.wav");
    let mp3 = work.path().join("Music sample.mp3");
    tone(&wave)?;
    make_mp3(&wave, &mp3)?;
    let source_before = fs::read(&wave)?;
    let source_mp3_before = fs::read(&mp3)?;
    let source = Span { start: S / 2, end: 5 * S / 2 };
    let fade = AudioFade::for_range(source, 3 * S / 5, 3 * S / 5)?;
    let mut base = Project::default();
    base.canvas = Canvas { width: 640, height: 360, fps: 30 };
    base.color([7, 35, 130], 3 * S)?;

    for (path, role, label) in [(&wave, SoundRole::Voice, "WAV voice"), (&mp3, SoundRole::Music, "MP3 music")] {
        let mut project = base.clone();
        let asset = project.import(path.clone(), editor::probe(path)?)?;
        let item = project.add_sound(asset, role, S / 4)?;
        project.sounds[0].range = source;
        project.sounds[0].audio_fade = fade;
        let output = work.path().join(format!("{label}.mp4"));
        let rendered = export(&project, &output)?;
        envelope(&rendered, 0.25, label)?;
        assert!(rms(&rendered, 0.05, 0.15)? < 0.005);
        if role == SoundRole::Voice {
            let slice = project.slice(Span { start: S / 2, end: 2 * S })?;
            let preview = export(&slice, &work.path().join("Sliced preview.mp4"))?;
            let final_gain = rms(&rendered, 0.54, 0.61)?;
            let preview_gain = rms(&preview, 0.04, 0.11)?;
            eprintln!("sliced mid-fade RMS {preview_gain:.5}, export RMS {final_gain:.5}");
            assert!((preview_gain / final_gain - 1.0).abs() < 0.17);

            project.split(item, S / 2)?;
            let split = export(&project, &work.path().join("Split edit.mp4"))?;
            let actual = rms(&split, 0.54, 0.61)?;
            eprintln!("split mid-fade RMS {actual:.5}, unsplit RMS {final_gain:.5}");
            assert!((actual / final_gain - 1.0).abs() < 0.17);
        }
    }

    // Make an imported flat video with a steady, native embedded audio track.
    let mut original_video = base.clone();
    let asset = original_video.import(wave.clone(), editor::probe(&wave)?)?;
    original_video.add_sound(asset, SoundRole::Effect, 0)?;
    let original_path = work.path().join("Imported video.mp4");
    export(&original_video, &original_path)?;
    let original_video_before = fs::read(&original_path)?;
    let mut shot = Project::default();
    shot.canvas = base.canvas;
    let asset = shot.import(original_path.clone(), editor::probe(&original_path)?)?;
    shot.append(asset)?;
    shot.shots[0].range = source;
    shot.shots[0].audio_fade = fade;
    let rendered = export(&shot, &work.path().join("Faded video.mp4"))?;
    envelope(&rendered, 0.0, "Imported video embedded sound")?;

    let mut extracted = base;
    let asset = extracted.import(original_path.clone(), editor::probe(&original_path)?)?;
    extracted.add_sound(asset, SoundRole::Effect, S / 4)?;
    extracted.sounds[0].range = source;
    extracted.sounds[0].audio_fade = fade;
    let rendered = export(&extracted, &work.path().join("Faded extracted sound.mp4"))?;
    envelope(&rendered, 0.25, "Audio from imported video")?;

    assert_eq!(fs::read(&wave)?, source_before);
    assert_eq!(fs::read(&mp3)?, source_mp3_before);
    assert_eq!(fs::read(&original_path)?, original_video_before);
    Ok(())
}

fn fragment_with_full_mix(reader: &Mp4Reader, path: &Path) -> Result<()> {
    use cv_media::{
        mp4::{frag::{FragWriter, TrackRun}, SampleRef, TrackSpec},
        recorder::{SegmentMeta, TrackMeta},
    };
    let video = reader.video().context("The source has no video")?;
    let audio = *reader.audio().first().context("The source has no audio")?;
    let mut groups = Vec::new();
    for (index, name) in [(video, "Video"), (audio, "Full mix")] {
        let track = &reader.tracks[index];
        let frames = reader.samples(index, 0, u64::MAX)?;
        let spec = TrackSpec {
            id: track.id,
            timescale: track.timescale,
            codec: track.codec.clone(),
            width: track.width.try_into()?,
            height: track.height.try_into()?,
            name: name.into(),
            enabled: true,
            default_duration: frames[0].duration,
        };
        let data = frames.iter().map(|sample| reader.read(sample)).collect::<Result<Vec<_>, _>>()?;
        groups.push((spec, frames, data));
    }
    let specs: Vec<_> = groups.iter().map(|(spec, _, _)| spec.clone()).collect();
    let tracks = vec![TrackMeta { id: specs[1].id, role: "full".into(), slot: None, app: None, name: "Full mix".into() }];
    let meta = serde_json::to_vec(&SegmentMeta { tracks, ..Default::default() })?;
    let refs: Vec<Vec<_>> = groups.iter().map(|(_, frames, data)| frames.iter().zip(data).map(|(sample, data)| SampleRef {
        data, duration: sample.duration, sync: sample.sync, cts: sample.cts,
    }).collect()).collect();
    let runs: Vec<_> = groups.iter().zip(&refs).map(|((spec, frames, _), samples)| TrackRun {
        track_id: spec.id, base_time: frames[0].dts, samples,
    }).collect();
    let mut writer = FragWriter::create(path, &specs, 0, Some(&meta))?;
    writer.write_fragment(&runs)?;
    writer.finish()?;
    Ok(())
}

#[test]
#[ignore = "renders a deterministic in-project fMP4 with a ClipVault Full mix track"]
fn applies_a_fade_to_the_fragmented_full_mix() -> Result<()> {
    let root = fixture_root()?;
    let work = tempfile::Builder::new().prefix("native-fade-recording-").tempdir_in(&root)?;
    let wave = work.path().join("Steady source.wav");
    tone(&wave)?;
    let mut base = Project::default();
    base.canvas = Canvas { width: 640, height: 360, fps: 30 };
    base.color([45, 35, 130], 3 * S)?;
    let asset = base.import(wave.clone(), editor::probe(&wave)?)?;
    base.add_sound(asset, SoundRole::Effect, 0)?;
    let source = export(&base, &work.path().join("Flat steady source.mp4"))?;
    let path = work.path().join("Fragmented Full mix source.mp4");
    fragment_with_full_mix(&source, &path)?;
    let recorder = cv_media::probe::probe(&path)?;
    assert!(recorder.fragmented && recorder.audio.iter().any(|track| track.role == cv_media::probe::TrackRole::Full));
    let source_before = fs::read(&path)?;

    let mut project = Project::default();
    project.canvas = base.canvas;
    let asset = project.import(path.clone(), editor::probe(&path)?)?;
    project.append(asset)?;
    let range = Span { start: S / 2, end: 5 * S / 2 };
    project.shots[0].range = range;
    project.shots[0].audio_fade = AudioFade::for_range(range, 3 * S / 5, 3 * S / 5)?;
    let faded = export(&project, &work.path().join("Faded full mix.mp4"))?;
    envelope(&faded, 0.0, "Fragmented ClipVault Full mix")?;
    assert_eq!(fs::read(&path)?, source_before);
    Ok(())
}

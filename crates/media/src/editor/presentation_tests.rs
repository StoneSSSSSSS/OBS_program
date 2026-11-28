use super::{build, render, render_native, Apartment, RenderControl};
use crate::{mp4::reader::Mp4Reader, probe};
use anyhow::{ensure, Context, Result};
use cv_core::editor::{AudioFade, Canvas, Project, SoundRole, Span, TICKS_PER_SECOND as S};
use std::{fs, io::{BufWriter, Write}, path::Path, process::Command};

fn tone_frame(i: u32) -> [i16; 2] {
    let t = f64::from(i) / 48_000.0;
    let sine = |hz: f64| (t * hz * std::f64::consts::TAU).sin();
    [(sine(313.0) * 13_000.0).round() as i16, (sine(677.0) * 5700.0).round() as i16]
}

fn write_tone(path: &Path) -> Result<()> {
    let mut output = BufWriter::new(fs::File::create_new(path)?);
    let frames = 15 * 48_000u32;
    let bytes = frames * 4;
    output.write_all(b"RIFF")?;
    output.write_all(&(36 + bytes).to_le_bytes())?;
    output.write_all(b"WAVEfmt ")?;
    output.write_all(&16u32.to_le_bytes())?;
    output.write_all(&1u16.to_le_bytes())?;
    output.write_all(&2u16.to_le_bytes())?;
    output.write_all(&48_000u32.to_le_bytes())?;
    output.write_all(&192_000u32.to_le_bytes())?;
    output.write_all(&4u16.to_le_bytes())?;
    output.write_all(&16u16.to_le_bytes())?;
    output.write_all(b"data")?;
    output.write_all(&bytes.to_le_bytes())?;
    for i in 0..frames {
        for channel in tone_frame(i) {
            output.write_all(&channel.to_le_bytes())?;
        }
    }
    output.flush()?;
    Ok(())
}

fn ffmpeg(source: &Path, input_options: &[&str], output_options: &[&str]) -> Result<(Vec<u8>, String)> {
    let executable = std::env::var_os("CLIPVAULT_EDITOR_FFMPEG")
        .context("Set CLIPVAULT_EDITOR_FFMPEG to the independent decoder")?;
    let command = Command::new(executable).args(["-hide_banner", "-nostdin"]).args(input_options)
        .arg("-i").arg(source).args(output_options).output()?;
    let log = String::from_utf8_lossy(&command.stderr).into_owned();
    ensure!(command.status.success(), "Independent FFmpeg decode failed: {log}");
    Ok((command.stdout, log))
}

fn stereo(source: &Path) -> Result<Vec<[f32; 2]>> {
    let (bytes, _) = ffmpeg(source, &["-v", "error"], &[
        "-map", "0:a:0", "-t", "12", "-c:a", "pcm_f32le", "-ar", "48000", "-ac", "2", "-f", "f32le", "pipe:1",
    ])?;
    Ok(bytes.chunks_exact(8).map(|frame| [
        f32::from_le_bytes(frame[..4].try_into().unwrap()),
        f32::from_le_bytes(frame[4..].try_into().unwrap()),
    ]).collect())
}

fn rms(samples: &[[f32; 2]], channel: usize, start: f64, end: f64) -> f64 {
    let a = (start * 48_000.0).round() as usize;
    let b = (end * 48_000.0).round() as usize;
    (samples[a..b].iter().map(|frame| f64::from(frame[channel]).powi(2)).sum::<f64>() / (b - a) as f64).sqrt()
}

#[test]
#[ignore = "uses the installed independent decoder and freshly generated synthetic stereo audio"]
fn exact_audio_presentation_preserves_native_stereo_fade_packets_and_picture_cut() -> Result<()> {
    let qa = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.clipvault-local/qa").canonicalize()?;
    let work = tempfile::Builder::new().prefix("native-presentation-").tempdir_in(qa)?;
    let input = work.path().join("Distinct stereo tones.wav");
    write_tone(&input)?;
    let input_bytes = fs::read(&input)?;
    let mut project = Project::default();
    project.canvas = Canvas { width: 640, height: 360, fps: 30 };
    project.color([20, 60, 170], 4 * S)?;
    project.color([170, 44, 20], 8 * S)?;
    let asset = project.import(input.clone(), super::probe(&input)?)?;
    project.add_sound(asset, SoundRole::Voice, S / 2)?;
    let sound = &mut project.sounds[0];
    sound.range = Span { start: S, end: 25 * S / 2 };
    sound.audio_fade = AudioFade::for_range(sound.range, S, 3 * S / 2)?;
    sound.gain = 0.8;
    let document = work.path().join("Stereo presentation.cvproj");
    project.save(&document)?;
    let document_bytes = fs::read(&document)?;
    let reopened = Project::load(&document)?;
    assert_eq!(reopened.duration(), 12 * S);

    let _apartment = Apartment::enter()?;
    let raw_scratch = tempfile::Builder::new().prefix("encoder-").tempdir_in(work.path())?;
    let raw_native = work.path().join("Raw native.mp4");
    let control = RenderControl::default();
    let composition = build(&reopened, raw_scratch.path(), &control)?;
    render_native(&composition, &raw_native, reopened.canvas, &control)?;
    drop(composition);
    let before = Mp4Reader::open(&raw_native)?;
    let source_track = before.audio()[0];
    assert!(before.end_ticks(source_track) > 576_000);

    let direct = work.path().join("Metadata repaired copy.mp4");
    fs::copy(&raw_native, &direct)?;
    crate::mp4::presentation::limit(&direct, 12 * S)?;
    let corrected = Mp4Reader::open(&direct)?;
    assert!((corrected.duration_secs() - 12.0).abs() < 1e-8);
    assert_eq!(corrected.tracks[corrected.audio()[0]].edit, Some((0, 576_000)));
    let originals = before.samples(source_track, 0, u64::MAX)?;
    let repairs = corrected.samples(corrected.audio()[0], 0, u64::MAX)?;
    assert_eq!(originals, repairs);
    for (a, b) in originals.iter().zip(&repairs) {
        assert_eq!(before.read(a)?, corrected.read(b)?);
    }
    let inspect = ["-map", "0:a:0", "-c:a", "copy", "-f", "framecrc", "pipe:1"];
    assert_eq!(ffmpeg(&raw_native, &["-v", "error", "-ignore_editlist", "1"], &inspect)?.0,
        ffmpeg(&direct, &["-v", "error", "-ignore_editlist", "1"], &inspect)?.0);
    let raw_samples = stereo(&raw_native)?;
    assert_eq!(raw_samples, stereo(&direct)?);
    eprintln!("Preserved {} AAC packets, including the final padded packet, and all 576000 presented stereo samples", originals.len());

    let delivery = work.path().join("Stereo 12 seconds.mp4");
    render(&reopened, &delivery, &RenderControl::default())?;
    assert!((probe::probe(&delivery)?.duration - 12.0).abs() < 1e-8);
    let (_, trace) = ffmpeg(&delivery, &["-loglevel", "trace"], &["-map", "0:a:0", "-t", "0.001", "-c:a", "copy", "-f", "null", "-"])?;
    assert!(trace.contains("stream 1: start_time: 0 duration: 12"), "{trace}");
    assert!(trace.contains("format: start_time: 0 duration: 12 ("), "{trace}");
    let delivered = stereo(&delivery)?;
    let source = stereo(&input)?;
    assert_eq!(delivered.len(), 576_000);
    for (channel, label) in [(0, "left"), (1, "right")] {
        let original = rms(&source, channel, 2.0, 4.0);
        let untrimmed = rms(&raw_samples, channel, 2.0, 4.0);
        let full = rms(&delivered, channel, 2.0, 4.0);
        let entering = rms(&delivered, channel, 0.62, 0.80);
        let leaving = rms(&delivered, channel, 11.72, 11.91);
        eprintln!("{label}: source {original:.5}, native {untrimmed:.5}, delivered {full:.5}, fade-in {entering:.5}, fade-out {leaving:.5}");
        assert!((full / untrimmed - 1.0).abs() < 0.01);
        assert!((full / original - 0.8).abs() < 0.15);
        assert!(entering > 0.05 * full && entering < 0.42 * full);
        assert!(leaving > 0.04 * full && leaving < 0.40 * full);
    }
    assert!(rms(&delivered, 0, 2.0, 4.0) / rms(&delivered, 1, 2.0, 4.0) > 2.0);

    let (pixels, _) = ffmpeg(&delivery, &["-v", "error"], &[
        "-map", "0:v:0", "-vf", r"select=eq(n\,119)+eq(n\,120),scale=2:2", "-frames:v", "2",
        "-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1",
    ])?;
    assert_eq!(pixels.len(), 24);
    assert!(i16::from(pixels[2]) - i16::from(pixels[0]) > 70);
    assert!(i16::from(pixels[12]) - i16::from(pixels[14]) > 70);
    assert_eq!(fs::read(&input)?, input_bytes);
    assert_eq!(fs::read(&document)?, document_bytes);
    Ok(())
}

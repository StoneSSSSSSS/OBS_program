//! Synthetic inputs for independent Editor exercises. Every counter is a source frame number.
use super::*;

#[derive(Clone, Copy)]
enum Scene { Court, Replay, Screen, Counter }

fn rect(frame: &mut [u8], x: usize, y: usize, width: usize, height: usize, luma: u8) {
    let stride = WIDTH as usize;
    let right = x.saturating_add(width).min(stride);
    if right <= x { return; }
    for row in y..y.saturating_add(height).min(HEIGHT as usize) {
        frame[row * stride + x..row * stride + right].fill(luma);
    }
}

fn number(frame: &mut [u8], value: usize, x: usize, y: usize, scale: usize) {
    const SEVEN_SEGMENTS: [u8; 10] = [0x3f, 0x06, 0x5b, 0x4f, 0x66, 0x6d, 0x7d, 0x07, 0x7f, 0x6f];
    let segments = [(1,0,3,1),(4,1,1,3),(4,5,1,3),(1,8,3,1),(0,5,1,3),(0,1,1,3),(1,4,3,1)];
    for (digit, power) in [100,10,1].into_iter().enumerate() {
        let bits = SEVEN_SEGMENTS[(value / power) % 10];
        for (bit, &(dx,dy,w,h)) in segments.iter().enumerate() {
            if bits & (1 << bit) != 0 {
                rect(frame, x + (digit * 7 + dx) * scale, y + dy * scale, w * scale, h * scale, 234);
            }
        }
    }
}

fn scene_frame(scene: Scene, index: usize) -> Vec<u8> {
    let pixels = (WIDTH * HEIGHT) as usize;
    let mut frame = vec![128; pixels + pixels / 2];
    let (base, u, v) = match scene {
        Scene::Court => (74, 103, 87),
        Scene::Replay => (90, 90, 151),
        Scene::Screen => (39, 150, 118),
        Scene::Counter => (27, 128, 128),
    };
    frame[..pixels].fill(base);
    for chroma in frame[pixels..].chunks_exact_mut(2) {
        chroma.copy_from_slice(&[u,v]);
    }
    match scene {
        Scene::Court | Scene::Replay => {
            rect(&mut frame, 12, 36, 296, 2, 195);
            rect(&mut frame, 12, 169, 296, 2, 195);
            rect(&mut frame, 12, 36, 2, 135, 195);
            rect(&mut frame, 306, 36, 2, 135, 195);
            rect(&mut frame, 159, 36, 2, 135, 195);
            let travel = index % 140;
            let x = if matches!(scene, Scene::Court) { 18 + travel * 2 } else { 298 - travel * 2 };
            let bounce = (index % 48).abs_diff(24);
            let y = 110 - bounce * 2;
            rect(&mut frame, x, y, 7, 7, 235);
            rect(&mut frame, (x + 20).min(296), 129, 7, 21, 31);
            for n in 0..(index / 70).min(6) {
                rect(&mut frame, 20 + n * 10, 13, 7, 10, 225);
            }
            rect(&mut frame, 246, 4, 66, 27, 30);
            number(&mut frame, index, 255, 8, 2);
        }
        Scene::Screen => {
            rect(&mut frame, 8, 12, 304, 164, 174);
            rect(&mut frame, 8, 12, 304, 19, 68);
            rect(&mut frame, 14, 37, 60, 132, 86);
            for row in 0..6 {
                rect(&mut frame, 85, 43 + row * 17, 100 + (row * 13) % 90, 4, 105);
            }
            let x = 32 + index % 235;
            let y = 45 + index / 7 % 90;
            rect(&mut frame, x, y, 4, 13, 25);
            rect(&mut frame, x + 3, y + 6, 5, 4, 25);
            number(&mut frame, index, 247, 146, 2);
        }
        Scene::Counter => {
            number(&mut frame, index, 70, 49, 9);
            rect(&mut frame, 12, 162, 3 * (index % 100), 8, 177);
        }
    }
    frame
}

fn write_wave(path: &Path, rate: u32, channels: u16, samples: u32, signal: impl Fn(f64, u16) -> f32) -> Result<()> {
    use std::io::{BufWriter, Write};
    let file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    let mut writer = BufWriter::new(file);
    let bytes = samples * u32::from(channels) * 2;
    writer.write_all(b"RIFF")?;
    writer.write_all(&(36 + bytes).to_le_bytes())?;
    writer.write_all(b"WAVEfmt ")?;
    writer.write_all(&16u32.to_le_bytes())?;
    writer.write_all(&1u16.to_le_bytes())?;
    writer.write_all(&channels.to_le_bytes())?;
    writer.write_all(&rate.to_le_bytes())?;
    writer.write_all(&(rate * u32::from(channels) * 2).to_le_bytes())?;
    writer.write_all(&(channels * 2).to_le_bytes())?;
    writer.write_all(&16u16.to_le_bytes())?;
    writer.write_all(b"data")?;
    writer.write_all(&bytes.to_le_bytes())?;
    for i in 0..samples {
        let time = f64::from(i) / f64::from(rate);
        for channel in 0..channels {
            writer.write_all(&((signal(time, channel).clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())?;
        }
    }
    writer.flush()?;
    Ok(())
}

fn alignment_card(path: &Path, width: u32, height: u32) -> Result<()> {
    anyhow::ensure!(!path.exists(), "existing input {}", path.display());
    let image = image::RgbImage::from_fn(width, height, |x, y| {
        let edge = x.min(width - 1 - x).min(y.min(height - 1 - y));
        let dx = x.abs_diff(width / 2);
        let dy = y.abs_diff(height / 2);
        let circle = (dx * dx + dy * dy).abs_diff(48 * 48) < 130;
        let value = if (8..=10).contains(&edge) || circle || (dx < 2 && dy < 60) || (dy < 2 && dx < 60) { 232 } else { 32 };
        image::Rgb([value, value, value])
    });
    image.save(path)?;
    Ok(())
}

#[test]
#[ignore = "writes generated stills, music and a cue to CLIPVAULT_EDITOR_INPUTS"]
fn generate_editor_audio_and_stills() -> Result<()> {
    let dir = review_dir()?;
    let rate = 48_000;
    write_wave(&dir.join("Night drive music.wav"), rate, 2, 16 * rate, |t, channel| {
        let notes = [220.0, 261.63, 329.63, 293.66, 196.0, 246.94, 293.66, 261.63];
        let f = notes[(t * 2.0) as usize % notes.len()];
        let beat = (t * 2.0).fract();
        let envelope = (beat * 30.0).min(1.0) * (1.0 - beat).powf(1.6);
        (tone(f, t) * 0.17 + tone(f * 2.0 + f64::from(channel), t) * 0.05) * envelope as f32
    })?;
    write_wave(&dir.join("Soft cue.wav"), rate, 1, rate * 65 / 100, |t, _| {
        tone(880.0, t) * (0.38 * (-8.0 * t).exp() * (t * 200.0).min(1.0)) as f32
    })?;
    alignment_card(&dir.join("Portrait alignment.png"), 360, 640)?;
    alignment_card(&dir.join("Landscape alignment.png"), 640, 360)?;
    Ok(())
}

fn review_dir() -> Result<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("CLIPVAULT_EDITOR_INPUTS").context("choose a review input directory")?);
    let local = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.clipvault-local").canonicalize()?;
    anyhow::ensure!(dir.canonicalize()?.starts_with(local), "review inputs must be inside the project");
    Ok(dir)
}

#[test]
#[ignore = "writes synthetic review inputs to CLIPVAULT_EDITOR_INPUTS"]
fn generate_editor_review_inputs() -> Result<()> {
    let dir = review_dir()?;
    for (scene, name, fragments) in [
        (Scene::Court, "Court wide.mp4", 15),
        (Scene::Replay, "Court replay.mp4", 10),
        (Scene::Screen, "Tutorial capture.mp4", 15),
        (Scene::Counter, "Frame counter.mp4", 5),
    ] {
        let path = dir.join(name);
        anyhow::ensure!(!path.exists(), "review source already exists: {}", path.display());
        let signal = |_: usize, t: f64| match scene {
            Scene::Court if ((t * 25.0) as usize % 70) < 2 => tone(620.0, t) * 0.18,
            Scene::Replay if ((t * 25.0) as usize % 70) < 2 => tone(830.0, t) * 0.18,
            _ => 0.0,
        };
        let fixture = Fixture::make_with_images(fragments, &["Original sound"], signal, |i| scene_frame(scene, i))?;
        fixture.write_flat(&path)?;
        let reader = Mp4Reader::open(&path)?;
        anyhow::ensure!((reader.duration_secs() - FRAGMENT_SECONDS * fragments as f64).abs() < 0.1);
        println!("{}: {:.3}s, {} frames", name, reader.duration_secs(), fixture.video.len());
    }
    Ok(())
}

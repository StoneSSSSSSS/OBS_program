use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const TONE_PEAK: f32 = 0.15;
pub const SIGNAL_FLOOR: f32 = 0.02;

pub fn write_tone(path: &Path, seconds: f32) {
    let frames = (48_000.0 * seconds) as usize;
    let bytes = (frames * 4) as u32;
    let mut wav = Vec::with_capacity(bytes as usize + 44);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&2_u16.to_le_bytes()); // stereo
    wav.extend_from_slice(&48_000_u32.to_le_bytes());
    wav.extend_from_slice(&192_000_u32.to_le_bytes());
    wav.extend_from_slice(&4_u16.to_le_bytes()); // bytes per frame
    wav.extend_from_slice(&16_u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&bytes.to_le_bytes());
    for frame in 0..frames {
        let angle = frame as f32 * 660.0 * std::f32::consts::TAU / 48_000.0;
        let pcm = (TONE_PEAK * angle.sin() * f32::from(i16::MAX)) as i16;
        wav.extend_from_slice(&pcm.to_le_bytes());
        wav.extend_from_slice(&pcm.to_le_bytes());
    }
    std::fs::write(path, wav).unwrap();
}

/// Use a separate executable so the recorder can attribute its audio to a PID.
pub struct TonePlayer(Child);

impl TonePlayer {
    pub fn start(path: &Path) -> Self {
        let child = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$ErrorActionPreference = 'Stop'; $player = [System.Media.SoundPlayer]::new($env:CLIPVAULT_TEST_TONE); $player.Load(); $player.PlaySync()",
            ])
            .env("CLIPVAULT_TEST_TONE", path)
            .stdin(Stdio::null())
            .spawn()
            .expect("start the tone player");
        Self(child)
    }

    pub fn finish(mut self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(status) = self.0.try_wait().expect("check the tone player") {
                assert!(status.success(), "tone playback exited with {status}");
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("tone playback did not finish within 10 seconds");
    }
}

impl Drop for TonePlayer {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

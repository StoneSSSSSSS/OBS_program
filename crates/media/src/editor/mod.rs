//! WinRT composition for the multi-clip editor.

mod cadence;
mod captions;
mod fades;
mod reframe;
#[cfg(test)]
mod presentation_tests;
mod stills;
mod title;
mod visual_transforms;

use anyhow::{bail, ensure, Context, Result};
use cv_core::editor::{AssetInfo, AssetKind, Canvas, Project, Source, Span, TICKS_PER_SECOND};
use image::RgbaImage;
use std::{
    fs,
    os::windows::ffi::OsStrExt,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Duration,
};
use windows::{
    core::{HSTRING, PCWSTR},
    Foundation::{AsyncOperationProgressHandler, AsyncStatus, TimeSpan},
    Media::{
        Editing::{
            BackgroundAudioTrack, MediaClip, MediaComposition, MediaTrimmingPreference,
            VideoFramePrecision,
        },
        MediaProperties::{
            MediaEncodingProfile,
            VideoEncodingQuality,
        },
        Transcoding::TranscodeFailureReason,
    },
    Storage::{Streams::DataReader, StorageFile},
    Win32::{
        Storage::FileSystem::MoveFileW,
        System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED},
    },
};

struct Apartment;

impl Apartment {
    fn enter() -> Result<Self> {
        // Call on an editor worker so blocking WinRT operations have an MTA.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.context("Initialize Windows media")?;
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
    }
}

fn ticks(value: u64) -> Result<TimeSpan> {
    Ok(TimeSpan { Duration: value.try_into().context("Media timestamp exceeds Windows range")? })
}

fn positive_ticks(value: TimeSpan) -> Result<u64> {
    ensure!(value.Duration > 0, "Media has no playable duration");
    Ok(value.Duration as u64)
}

fn media_path(path: &Path) -> Result<HSTRING> {
    let absolute = std::path::absolute(path)?;
    // WinRT expects DOS/UNC path spelling even when the caller used a
    // canonical Windows extended-length path.
    let wide = absolute.as_os_str().encode_wide().collect::<Vec<_>>();
    let verbatim_unc = r"\\?\UNC\".encode_utf16().collect::<Vec<_>>();
    let verbatim_disk = r"\\?\".encode_utf16().collect::<Vec<_>>();
    let path = if let Some(unc) = wide.strip_prefix(verbatim_unc.as_slice()) {
        HSTRING::from_wide(&r"\\".encode_utf16().chain(unc.iter().copied()).collect::<Vec<_>>())?
    } else {
        HSTRING::from_wide(wide.strip_prefix(verbatim_disk.as_slice()).unwrap_or(&wide))?
    };
    Ok(path)
}

fn storage_file(path: &Path) -> Result<StorageFile> {
    StorageFile::GetFileFromPathAsync(&media_path(path)?)?
        .get()
        .with_context(|| format!("Open {}", path.display()))
}

fn picture_duration(reader: &crate::mp4::reader::Mp4Reader) -> Option<u64> {
    let (ticks, scale) = reader.picture_duration()?;
    let scale = u128::from(scale);
    let duration = (u128::from(ticks) * u128::from(TICKS_PER_SECOND) + scale / 2) / scale;
    u64::try_from(duration).ok().filter(|duration| (1..=i64::MAX as u64).contains(duration))
}

fn video_info(clip: &MediaClip, path: &Path) -> Result<AssetInfo> {
    let native_duration = positive_ticks(clip.OriginalDuration()?)?;
    let duration = crate::mp4::reader::Mp4Reader::open(path).ok()
        .and_then(|reader| picture_duration(&reader)).unwrap_or(native_duration).min(native_duration);
    let props = clip.GetVideoEncodingProperties()?;
    let (width, height) = (props.Width()?, props.Height()?);
    ensure!(width > 0 && height > 0, "Media has no video picture");
    Ok(AssetInfo {
        kind: AssetKind::Video,
        fps_milli: props.FrameRate().ok().and_then(|rate| {
            let n = u64::from(rate.Numerator().ok()?);
            let d = u64::from(rate.Denominator().ok()?);
            u32::try_from((n * 1000 + d / 2).checked_div(d)?).ok().filter(|rate| *rate > 0)
        }),
        duration,
        width,
        height,
    })
}

fn audio_info(file: &StorageFile) -> Result<AssetInfo> {
    let track = BackgroundAudioTrack::CreateFromFileAsync(file)?.get()?;
    Ok(AssetInfo {
        kind: AssetKind::Audio,
        fps_milli: None,
        duration: positive_ticks(track.OriginalDuration()?)?,
        width: 0,
        height: 0,
    })
}

/// Opens an input with the same Windows media APIs used to render it.
pub fn probe(path: &Path) -> Result<AssetInfo> {
    let _apartment = Apartment::enter()?;
    let file = storage_file(path)?;
    let ext = path.extension().and_then(|ext| ext.to_str()).unwrap_or("").to_ascii_lowercase();
    let result = match ext.as_str() {
        "png" | "jpg" | "jpeg" | "bmp" | "gif" | "tiff" | "tif" | "webp" | "heic" | "heif" => {
            stills::probe(&file)
        }
        "wav" | "mp3" | "flac" | "m4a" | "aac" | "wma" | "aiff" | "aif" | "opus" | "ogg" => audio_info(&file),
        _ => MediaClip::CreateFromFileAsync(&file)
            .and_then(|operation| operation.get())
            .map_err(anyhow::Error::from)
            .and_then(|clip| video_info(&clip, path))
            .or_else(|video_error| {
                if let Ok(reader) = crate::mp4::reader::Mp4Reader::open(path) {
                    let info = crate::probe::probe_reader(&reader);
                    if info.fragmented && info.video_codec == "h264" && info.duration > 0.0 {
                        return Ok(AssetInfo {
                            kind: AssetKind::Video,
                            fps_milli: (info.fps > 0.0).then(|| (info.fps * 1000.0).round() as u32),
                            duration: picture_duration(&reader)
                                .unwrap_or_else(|| (info.duration * TICKS_PER_SECOND as f64).round() as u64),
                            width: info.width,
                            height: info.height,
                        });
                    }
                }
                audio_info(&file).with_context(|| format!("Video import also failed: {video_error:#}"))
            }),
    };
    result.with_context(|| format!("Cannot import {}", path.display()))
}

/// Cancellation and normalized progress for one render.
#[derive(Default)]
pub struct RenderControl {
    cancelled: AtomicBool,
    commit: Mutex<()>,
    progress: Arc<AtomicU32>,
}

impl RenderControl {
    pub fn cancel(&self) {
        let _commit = self.commit();
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn progress(&self) -> f32 {
        f32::from_bits(self.progress.load(Ordering::Relaxed))
    }

    fn commit(&self) -> MutexGuard<'_, ()> {
        self.commit.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn check(&self) -> Result<()> {
        ensure!(!self.cancelled.load(Ordering::Acquire), "Editor export cancelled");
        Ok(())
    }
}

fn trim_duration(original: TimeSpan, range: Span) -> Result<TimeSpan> {
    let duration = positive_ticks(original)?;
    ensure!(range.start < range.end && range.end <= duration, "Source trim exceeds the available media");
    ticks(duration - range.end)
}

fn select_full_mix(clip: &MediaClip, path: &Path) -> Result<()> {
    // Native ClipVault files place the full mix before the microphone and app tracks.
    // Use the stored role to choose it when present, and preserve Windows' default
    // selection for other files.
    if let Ok(info) = crate::probe::probe(path) {
        if let Some(track) = info.audio.iter().find(|track| track.role == crate::probe::TrackRole::Full) {
            ensure!(track.index < clip.EmbeddedAudioTracks()?.Size()? as usize, "ClipVault full mix is missing");
            clip.SetSelectedEmbeddedAudioTrackIndex(track.index as u32)?;
        }
    }
    Ok(())
}

fn open_video(path: &Path, range: Span, work: &Path, id: u64, control: &RenderControl) -> Result<(MediaClip, Span, StorageFile)> {
    let file = storage_file(path)?;
    match MediaClip::CreateFromFileAsync(&file).and_then(|operation| operation.get()) {
        Ok(clip) => {
            select_full_mix(&clip, path)?;
            Ok((clip, range, file))
        }
        Err(native_error) => {
            let info = crate::probe::probe(path).with_context(|| format!("Windows could not open the video: {native_error}"))?;
            ensure!(info.fragmented && info.video_codec == "h264", "Windows could not open the video: {native_error}");
            // The Windows editing API needs a flat MP4 for ClipVault recordings.
            // Keep compressed samples and their presentation trims via the existing cutter.
            let flat = work.join(format!("source-{id}.mp4"));
            let source = crate::cut::CutSource {
                path: path.into(),
                from_s: range.start as f64 / TICKS_PER_SECOND as f64,
                to_s: Some(range.end as f64 / TICKS_PER_SECOND as f64),
            };
            crate::cut::cut(&[source], &flat, None, &mut |_| {}, &control.cancelled)
                .context("Prepare the recording for Windows media editing")?;
            let file = storage_file(&flat)?;
            let clip = MediaClip::CreateFromFileAsync(&file)?.get()?;
            select_full_mix(&clip, &flat)?;
            let actual = positive_ticks(clip.OriginalDuration()?)?;
            Ok((clip, Span { start: 0, end: actual.min(range.len()) }, file))
        }
    }
}

fn embedded_audio(clip: &MediaClip) -> Result<Option<BackgroundAudioTrack>> {
    let audio = clip.EmbeddedAudioTracks()?;
    if audio.Size()? == 0 {
        return Ok(None);
    }
    let selected = audio.GetAt(clip.SelectedEmbeddedAudioTrackIndex()?)?;
    Ok(Some(BackgroundAudioTrack::CreateFromEmbeddedAudioTrack(&selected)?))
}

fn build(project: &Project, work: &Path, control: &RenderControl) -> Result<MediaComposition> {
    let composition = MediaComposition::new()?;
    let clips = composition.Clips()?;
    let tracks = composition.BackgroundAudioTracks()?;
    let mut shot_at = 0;
    for shot in &project.shots {
        control.check()?;
        let mut shot_gain = shot.gain;
        let (clip, range) = (|| -> Result<(MediaClip, Span)> {
            let range = shot.range;
            let frame = match &shot.source {
                Source::Asset(id) => {
                    let asset = project.asset(*id).context("Shot source is missing")?;
                    match asset.info.kind {
                        AssetKind::Video => {
                            let (clip, range, file) = open_video(&asset.path, range, work, shot.id, control)?;
                            if let Some(fade) = shot.audio_fade.filter(|_| shot.gain > 0.0) {
                                let path = work.join(format!("fade-shot-{}.wav", shot.id));
                                if let Some(track) = fades::stage(&asset.path, true, shot.range, fade, &path, control)? {
                                    track.SetDelay(ticks(shot_at)?)?;
                                    track.SetVolume(shot.gain)?;
                                    tracks.Append(&track)?;
                                    shot_gain = 0.0;
                                }
                            }
                            if !shot.visual_transform.is_identity() {
                                // Retain the exact embedded audio selection, including ClipVault's full mix.
                                if shot_gain > 0.0 {
                                    if let Some(track) = embedded_audio(&clip)? {
                                        let original = track.OriginalDuration()?;
                                        let audio_range = Span {
                                            end: range.end.min(positive_ticks(original)?),
                                            ..range
                                        };
                                        if !audio_range.is_empty() {
                                            track.SetTrimTimeFromStart(ticks(audio_range.start)?)?;
                                            track.SetTrimTimeFromEnd(trim_duration(original, audio_range)?)?;
                                            track.SetDelay(ticks(shot_at)?)?;
                                            track.SetVolume(shot_gain)?;
                                            tracks.Append(&track)?;
                                        }
                                    }
                                }
                                return visual_transforms::video(
                                    &file, &clip, range, shot.visual_transform, project.canvas,
                                    &work.join(format!("picture-{}.mp4", shot.id)), control,
                                );
                            }
                            return Ok((clip, range));
                        }
                        AssetKind::Image => stills::decode(
                            &storage_file(&asset.path)?, project.canvas, shot.visual_transform,
                        )?,
                        AssetKind::Audio => bail!("A visual shot cannot use an audio source"),
                    }
                }
                Source::Title(text) => title::draw(project.canvas, text)?,
                Source::Color(color) => stills::solid(project.canvas, *color),
            };
            Ok((stills::clip(frame, ticks(range.end)?, &work.join(format!("still-{}.jpg", shot.id)))?, range))
        })().with_context(|| format!("Open visual shot {}", shot.id))?;
        clip.SetTrimTimeFromStart(ticks(range.start)?)?;
        clip.SetTrimTimeFromEnd(trim_duration(clip.OriginalDuration()?, range)?)?;
        clip.SetVolume(shot_gain)?;
        clips.Append(&clip)?;
        shot_at += shot.range.len();
    }

    let tail = project.duration().saturating_sub(project.visual_duration());
    if tail > 0 {
        let frame = stills::solid(project.canvas, [0, 0, 0]);
        clips.Append(&stills::clip(frame, ticks(tail)?, &work.join("audio-tail.jpg"))?)?;
    }

    for sound in &project.sounds {
        control.check()?;
        let asset = project.asset(sound.asset).context("Sound source is missing")?;
        if let Some(fade) = sound.audio_fade {
            if sound.gain == 0.0 {
                continue;
            }
            let path = work.join(format!("fade-sound-{}.wav", sound.id));
            let track = fades::stage(&asset.path, asset.info.kind == AssetKind::Video, sound.range, fade, &path, control)?
                .context("The selected sound source has no audio")?;
            track.SetDelay(ticks(sound.at)?)?;
            track.SetVolume(sound.gain)?;
            tracks.Append(&track)?;
            continue;
        }
        let (track, mut range) = (|| -> Result<(BackgroundAudioTrack, Span)> {
            if asset.info.kind == AssetKind::Video {
                let (clip, range, _) = open_video(&asset.path, sound.range, work, sound.id, control)?;
                let audio = embedded_audio(&clip)?.context("The video has no audio tracks")?;
                return Ok((audio, range));
            }
            let input = storage_file(&asset.path)?;
            Ok((BackgroundAudioTrack::CreateFromFileAsync(&input)?.get()?, sound.range))
        })().with_context(|| format!("Open audio {}", asset.path.display()))?;
        let original = track.OriginalDuration()?;
        if asset.info.kind == AssetKind::Video {
            range.end = range.end.min(positive_ticks(original)?);
            if range.start >= range.end {
                continue;
            }
        }
        track.SetTrimTimeFromStart(ticks(range.start)?)?;
        track.SetTrimTimeFromEnd(trim_duration(original, range)?)?;
        track.SetDelay(ticks(sound.at)?)?;
        track.SetVolume(sound.gain)?;
        tracks.Append(&track)?;
    }
    captions::apply(&composition, project, work, control)?;
    Ok(composition)
}


fn profile(canvas: Canvas) -> Result<MediaEncodingProfile> {
    let profile = MediaEncodingProfile::CreateMp4(VideoEncodingQuality::HD1080p)?;
    let video = profile.Video()?;
    video.SetWidth(canvas.width)?;
    video.SetHeight(canvas.height)?;
    video.FrameRate()?.SetNumerator(canvas.fps)?;
    video.FrameRate()?.SetDenominator(1)?;
    video.PixelAspectRatio()?.SetNumerator(1)?;
    video.PixelAspectRatio()?.SetDenominator(1)?;
    let bitrate = (u64::from(canvas.width) * u64::from(canvas.height) * u64::from(canvas.fps) / 8)
        .clamp(1_500_000, 40_000_000) as u32;
    video.SetBitrate(bitrate)?;
    profile.SetVideo(&video)?;
    Ok(profile)
}

fn render_native(composition: &MediaComposition, path: &Path, canvas: Canvas, control: &RenderControl) -> Result<()> {
    control.check()?;
    fs::File::create_new(path)?;
    let destination = storage_file(path)?;
    let profile = profile(canvas)?;
    let operation = composition.RenderToFileWithProfileAsync(&destination, MediaTrimmingPreference::Precise, &profile)
        .context("Start the Windows MP4 encoder")?;
    let progress = Arc::clone(&control.progress);
    let _ = operation.SetProgress(&AsyncOperationProgressHandler::new(move |_, value: &f64| {
        if value.is_finite() {
            let fraction = (value / 100.0).clamp(0.0, 0.99) as f32;
            progress.fetch_max(fraction.to_bits(), Ordering::Relaxed);
        }
        Ok(())
    }));

    let mut cancellation_sent = false;
    let status = loop {
        let status = operation.Status()?;
        if status != AsyncStatus::Started {
            break status;
        }
        if !cancellation_sent && control.check().is_err() {
            // Continue waiting for a terminal status before removing staging files.
            let _ = operation.Cancel();
            cancellation_sent = true;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let result = operation.GetResults();
    let _ = operation.Close();
    control.check()?;
    ensure!(status != AsyncStatus::Canceled, "Editor export cancelled");
    let reason = result.context("Windows MP4 encoding failed")?;
    ensure!(status == AsyncStatus::Completed && reason == TranscodeFailureReason::None,
        "Windows could not encode this composition ({reason:?})");
    Ok(())
}

/// Render an H.264/AAC MP4, publishing a complete file without overwriting.
pub fn render(project: &Project, destination: &Path, control: &RenderControl) -> Result<()> {
    project.validate()?;
    ensure!(project.duration() > 0, "The editor timeline is empty");
    control.check()?;
    let _apartment = Apartment::enter()?;
    let absolute = std::path::absolute(destination)?;
    ensure!(absolute.extension().and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("mp4")), "Choose an MP4 filename for the export");
    ensure!(!absolute.try_exists()?, "The export filename already exists");
    let parent = absolute.parent().context("Choose an export folder")?;
    let work = tempfile::Builder::new().prefix(".clipvault-editor-").tempdir_in(parent)?;
    let staged = work.path().join("render.mp4");
    {
        let composition = build(project, work.path(), control)?;
        render_native(&composition, &staged, project.canvas, control)?;
    }
    control.check()?;
    let remuxed = work.path().join("frame-grid.mp4");
    let staged = if cadence::normalize(&staged, &remuxed, project.canvas.fps, project.duration(), control)? {
        remuxed
    } else {
        staged
    };
    let info = crate::probe::probe(&staged).context("Check the rendered MP4")?;
    ensure!(
        info.video_codec == "h264" && info.width == project.canvas.width && info.height == project.canvas.height,
        "The rendered MP4 does not match the selected canvas",
    );
    let expected_duration = project.duration() as f64 / TICKS_PER_SECOND as f64;
    let tolerance = (2.0 / f64::from(project.canvas.fps)).max(0.05);
    ensure!(
        info.duration > 0.0 && (info.duration - expected_duration).abs() <= tolerance,
        "Windows produced {:.3} seconds for a {:.3}-second timeline", info.duration, expected_duration,
    );
    control.check()?;
    crate::mp4::presentation::limit(&staged, project.duration())
        .context("Set the exact presentation duration on the rendered MP4")?;
    fs::OpenOptions::new().write(true).open(&staged)?.sync_all()?;
    let from = HSTRING::from(staged.as_os_str());
    let to = HSTRING::from(absolute.as_os_str());
    let _commit = control.commit();
    control.check()?;
    unsafe { MoveFileW(PCWSTR(from.as_ptr()), PCWSTR(to.as_ptr())) }
        .with_context(|| format!("Publish without replacing {}", absolute.display()))?;
    control.progress.store(1.0f32.to_bits(), Ordering::Relaxed);
    Ok(())
}

/// Decode the nearest composed frame at the editor playhead on a media worker.
pub fn thumbnail(project: &Project, at: u64, width: u32, height: u32) -> Result<RgbaImage> {
    project.validate()?;
    ensure!(project.duration() > 0, "The editor timeline is empty");
    ensure!(width > 0 && height > 0 && width <= 4096 && height <= 4096, "Invalid preview size");
    let _apartment = Apartment::enter()?;
    let scratch = cv_core::paths::data_dir().join("editor-preview");
    fs::create_dir_all(&scratch)?;
    let work = tempfile::Builder::new().prefix("frame-").tempdir_in(scratch)?;
    let frame_length = TICKS_PER_SECOND.div_ceil(u64::from(project.canvas.fps));
    let at = at.min(project.duration().saturating_sub(frame_length));
    let window = Span {
        start: at.saturating_sub(TICKS_PER_SECOND / 2),
        end: at.saturating_add(TICKS_PER_SECOND / 2).min(project.duration()),
    };
    let mut nearby = project.slice(window)?;
    nearby.sounds.clear();
    nearby.captions.clear();
    for shot in &mut nearby.shots { shot.audio_fade = None; }
    let composition = build(&nearby, work.path(), &RenderControl::default())?;
    let image = composition.GetThumbnailAsync(
        ticks(at - window.start)?,
        width as i32,
        height as i32,
        VideoFramePrecision::NearestFrame,
    )?.get().context("Read the composed preview frame")?;
    let size = u32::try_from(image.Size()?).context("Preview frame is too large")?;
    let reader = DataReader::CreateDataReader(&image.GetInputStreamAt(0)?)?;
    ensure!(reader.LoadAsync(size)?.get()? == size, "The preview frame is incomplete");
    let mut bytes = vec![0; size as usize];
    reader.ReadBytes(&mut bytes)?;
    let mut frame = image::load_from_memory(&bytes)?.to_rgba8();
    captions::draw_at(&mut frame, project, at)?;
    Ok(frame)
}

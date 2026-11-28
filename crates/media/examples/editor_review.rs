//! Operator-only tools for disposable Editor review runs. Pass one JSON request on stdin.
use anyhow::{bail, ensure, Context, Result};
use cv_core::editor::{AssetKind, Canvas, Project, Span, TICKS_PER_SECOND};
use cv_media::mf::h264dec::{nv12_to_rgb, DecodedFrame, VideoDecoder};
use cv_media::mp4::{reader::Mp4Reader, Codec};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    fs::{self, File},
    io::{self, BufWriter, Read},
    os::windows::{fs::MetadataExt, io::AsRawHandle},
    path::{Component, Path, PathBuf},
};
use windows::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION},
};

const SEC: u64 = TICKS_PER_SECOND;
const WIDE: &str = "Court wide.mp4";
const REPLAY: &str = "Court replay.mp4";
const REPARSE_POINT: u32 = 0x400;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    run: PathBuf,
    operation: Operation,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Seed { scenario: Scenario },
    Inspect { output: PathBuf, frame_at: Vec<f64>, audio_ranges: Vec<[f64; 2]> },
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Scenario {
    Revision,
    LongAssembly,
    Relink,
}

fn main() -> Result<()> {
    let mut contents = Vec::new();
    io::stdin().take(16_385).read_to_end(&mut contents)?;
    ensure!(contents.len() <= 16_384, "Review request exceeds 16 KiB");
    let Request { run, operation } = serde_json::from_slice(&contents)?;
    let run = review_run(&run)?;
    let result = match operation {
        Operation::Seed { scenario } => seed(&run, scenario)?,
        Operation::Inspect { output, frame_at, audio_ranges } => {
            inspect(&run, &output, &frame_at, &audio_ranges)?
        }
    };
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

fn review_run(path: &Path) -> Result<PathBuf> {
    let project = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize()?;
    ensure!(project.join(".clipvault-project").is_file(), "Expected the ClipVault source project");
    let path = path.canonicalize()?;
    let expected_parent = project.join(".clipvault-local").join("qa").join("editor-reviews");
    ensure!(path.parent() == Some(expected_parent.as_path()), "Choose an isolated Editor review run");
    let name = path.file_name().and_then(|name| name.to_str()).context("Invalid review name")?;
    let (brief, suffix) = name.rsplit_once('-').context("Invalid review name")?;
    ensure!(
        !brief.is_empty() && brief.len() <= 40
            && brief.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            && suffix.len() == 8 && suffix.bytes().all(|c| c.is_ascii_hexdigit()),
        "Choose the Evidence folder printed by start-editor-review.ps1"
    );
    for folder in ["inputs", "output", "profile", "tmp"] {
        let dir = path.join(folder);
        let metadata = fs::symlink_metadata(&dir)?;
        ensure!(metadata.is_dir() && metadata.file_attributes() & REPARSE_POINT == 0,
            "Invalid review directory {}", dir.display());
    }
    Ok(path)
}

fn private_file(run: &Path, relative: &Path) -> Result<PathBuf> {
    ensure!(relative.components().all(|c| matches!(c, Component::Normal(_))),
        "Choose a relative path inside this run");
    let full = run.join(relative);
    let resolved = full.canonicalize()?;
    ensure!(resolved == full && full != run, "Choose a regular file inside this run");
    let file = File::open(&full)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // The owned File keeps this HANDLE valid for the call.
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }?;
    ensure!(
        file.metadata()?.is_file() && info.nNumberOfLinks == 1
            && info.dwFileAttributes & REPARSE_POINT == 0,
        "Choose a regular, private file inside this run"
    );
    Ok(full)
}

fn fixture(run: &Path, name: &str) -> Result<PathBuf> {
    private_file(run, &Path::new("inputs").join(name))
}

fn seed(run: &Path, scenario: Scenario) -> Result<Value> {
    let wide = fixture(run, WIDE)?;
    let replay = fixture(run, REPLAY)?;
    let wide_info = cv_media::editor::probe(&wide)?;
    let replay_info = cv_media::editor::probe(&replay)?;
    ensure!(wide_info.kind == AssetKind::Video && wide_info.duration >= 8 * SEC
        && replay_info.kind == AssetKind::Video && replay_info.duration >= 7 * SEC,
        "The review requires the generated Court wide and Court replay footage");

    let mut project = Project::default();
    project.canvas = Canvas { width: 640, height: 360, fps: 25 };
    let (workspace, path) = match scenario {
        Scenario::Revision => {
            project.name = "Rough cut".into();
            let wide_id = project.import(wide, wide_info)?;
            let replay_id = project.import(replay, replay_info)?;
            append(&mut project, wide_id, SEC, 7 * SEC)?;
            append(&mut project, replay_id, SEC / 2, 5 * SEC)?;
            project.title("Final whistle".into(), 2 * SEC)?;
            let workspace = run.join("revision");
            let path = workspace.join("Rough cut.cvproj");
            (workspace, path)
        }
        Scenario::LongAssembly => {
            project.name = "Long assembly".into();
            let wide_id = project.import(wide, wide_info)?;
            let replay_id = project.import(replay, replay_info)?;
            project.title("Match review".into(), 2 * SEC)?;
            for index in 0..42 {
                let asset = if index % 2 == 0 { wide_id } else { replay_id };
                let start = if index % 4 < 2 { 0 } else { SEC };
                append(&mut project, asset, start, start + 6 * SEC)?;
            }
            project.title("End of review".into(), 3 * SEC)?;
            ensure!(project.duration() >= 240 * SEC, "Long fixture is under four minutes");
            let workspace = run.join("long-assembly");
            let path = workspace.join("Long assembly.cvproj");
            (workspace, path)
        }
        Scenario::Relink => {
            project.name = "Handoff".into();
            let workspace = run.join("handoff");
            let existing = workspace.join("footage");
            let old = workspace.join("replay media");
            let new = workspace.join("received media");
            fs::create_dir(&workspace)?;
            fs::create_dir(&existing)?;
            fs::create_dir(&old)?;
            fs::copy(wide, existing.join(WIDE))?;
            fs::copy(replay, old.join(REPLAY))?;
            let wide_id = project.import(existing.join(WIDE), wide_info)?;
            let replay_id = project.import(old.join(REPLAY), replay_info)?;
            append(&mut project, wide_id, SEC, 4 * SEC)?;
            append(&mut project, replay_id, 0, 4 * SEC)?;
            let path = workspace.join("Handoff.cvproj");
            project.save(&path)?;
            fs::rename(old, new)?;
            return verified_seed(run, &path, 1);
        }
    };
    fs::create_dir(workspace)?;
    project.save(&path)?;
    verified_seed(run, &path, 0)
}

fn append(project: &mut Project, asset: u64, start: u64, end: u64) -> Result<()> {
    project.append(asset)?;
    project.shots.last_mut().context("No appended shot")?.range = Span { start, end };
    Ok(())
}

fn verified_seed(run: &Path, path: &Path, missing: usize) -> Result<Value> {
    let project = Project::load(path)?;
    ensure!(project.assets.iter().filter(|asset| !asset.path.is_file()).count() == missing,
        "Generated project does not have the expected media availability");
    Ok(json!({ "project": path.strip_prefix(run)? }))
}

fn inspect(run: &Path, output: &Path, frame_at: &[f64], audio_ranges: &[[f64; 2]]) -> Result<Value> {
    ensure!(output.starts_with("output") && output.extension().is_some_and(|s| s.eq_ignore_ascii_case("mp4")),
        "Choose an MP4 under the review's output folder");
    ensure!(frame_at.len() <= 40 && audio_ranges.len() <= 40, "Use at most 40 frame or audio samples per request");
    let source = private_file(run, output)?;
    let reader = Mp4Reader::open(&source)?;
    let video = reader.video().context("No supported video track")?;
    let audio = reader.audio();
    let duration = reader.duration_secs();
    let display = cv_media::probe::probe_reader(&reader);
    let mut tracks = Vec::new();
    for (index, track) in reader.tracks.iter().enumerate() {
        tracks.push(json!({
            "number": index + 1, "kind": format!("{:?}", track.kind),
            "header_width": track.width, "header_height": track.height,
            "timescale": track.timescale, "sample_end_ticks": reader.end_ticks(index),
            "edit": track.edit,
        }));
    }

    let mut levels = Vec::new();
    for &[start, end] in audio_ranges {
        ensure!(start >= 0.0 && end > start && end <= duration + 0.001 && end - start <= 30.0,
            "Choose an audio range between 0 and the output duration, up to 30 seconds");
        for &index in &audio {
            let pcm = cv_media::audioread::read_range(&reader, index, start, end)?;
            ensure!(!pcm.is_empty(), "Audio sample window is too short");
            let sum = pcm.iter().map(|sample| f64::from(*sample).powi(2)).sum::<f64>();
            let peak = pcm.iter().map(|sample| sample.abs()).fold(0.0, f32::max);
            levels.push(json!({
                "track": index + 1, "start_seconds": start, "end_seconds": end,
                "rms": (sum / pcm.len() as f64).sqrt(), "peak": peak,
            }));
        }
    }

    let mut frames = Vec::new();
    if !frame_at.is_empty() {
        let work = tempfile::Builder::new().prefix("inspected-frames-").tempdir_in(run)?;
        for (index, &at) in frame_at.iter().enumerate() {
            ensure!(at >= 0.0 && at < duration, "Frame timestamp falls outside the export");
            let (frame, presented) = frame_at_time(&reader, video, at)?;
            let path = work.path().join(format!("frame-{index:02}.png"));
            let rgb = nv12_to_rgb(&frame, frame.width);
            rgb.write_to(&mut BufWriter::new(File::create_new(&path)?), image::ImageFormat::Png)?;
            frames.push(json!({
                "requested_seconds": at, "presented_seconds": presented,
                "image": path.strip_prefix(run)?,
            }));
        }
        let _ = work.keep();
    }

    Ok(json!({
        "file": output, "duration_seconds": duration,
        "width": display.width, "height": display.height, "fps": display.fps,
        "video_codec": display.video_codec,
        "tracks": tracks, "frames": frames, "audio": levels,
    }))
}

fn frame_at_time(reader: &Mp4Reader, video: usize, at: f64) -> Result<(DecodedFrame, f64)> {
    let track = &reader.tracks[video];
    let Codec::H264 { avcc } = &track.codec else { bail!("Frame inspection requires H.264") };
    let scale = i64::from(track.timescale);
    ensure!(scale > 0, "Video track has no timebase");
    let edit = track.edit.map(|(start, _)| start.max(0)).unwrap_or(0);
    let media_at = ((at * scale as f64).floor() as i64).saturating_add(edit);
    let from = media_at.saturating_sub(scale * 3).max(0) as u64;
    let to = media_at.saturating_add(scale * 3) as u64;
    let candidates = reader.samples(video, from, to)?;
    let selected = candidates.iter().filter(|sample| sample.pts() <= media_at)
        .max_by_key(|sample| sample.pts()).context("No video sample at this time")?;
    let seek = reader.sync_at_or_before(video, selected.dts)?.context("No sync sample before this frame")?;
    let samples = reader.samples(video, seek.dts, selected.dts.saturating_add(scale as u64 * 2))?;
    let hns = |pts: i64| (i128::from(pts) * i128::from(SEC) / i128::from(scale)) as i64;
    let target = hns(selected.pts());
    let mut decoder = VideoDecoder::new(avcc, None, false)?;
    let presented = (selected.pts() - edit).max(0) as f64 / scale as f64;
    for sample in samples {
        for frame in decoder.decode(&reader.read(&sample)?, hns(sample.pts()))? {
            if frame.pts_hns == target {
                return Ok((frame, presented));
            }
        }
    }
    let frame = decoder.drain()?.into_iter().find(|frame| frame.pts_hns == target)
        .context("Decoder did not return the selected presentation sample")?;
    Ok((frame, presented))
}

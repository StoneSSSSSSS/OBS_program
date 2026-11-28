use super::*;
use crate::player::{PlayState, Player};
use crate::service::Service;
use crate::thumbs::Thumbs;
use anyhow::{ensure, Context, Result};
use cv_core::config::Config;
use cv_core::db::Db;
use cv_media::cut::{cut, CutSource};
use std::path::Path;

fn render(ctx: &egui::Context, view: &mut RecordingsView, c: &mut Ctx, key: Option<egui::Key>) {
    let events = match key {
        Some(key) => [true, false].into_iter().map(|pressed| egui::Event::Key {
            key, physical_key: None, pressed, repeat: false, modifiers: egui::Modifiers::NONE,
        }).collect(),
        None => Vec::new(),
    };
    let _ = ctx.run_ui(egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::new(1182.0, 771.0))),
        events,
        ..Default::default()
    }, |ui| view.ui(ui, c));
}

fn wait_for(
    ctx: &egui::Context,
    view: &mut RecordingsView,
    c: &mut Ctx,
    path: &Path,
    label: &str,
    observed: impl Fn(&PlayState) -> bool,
) -> Result<()> {
    let timeout = Instant::now() + Duration::from_secs(8);
    loop {
        render(ctx, view, c, None);
        let state = c.player.state();
        let selected = c.player.path.as_deref().map(Path::new);
        if selected == Some(path) && state.loaded && state.paused && observed(&state) {
            println!("{label}: {:.3}s in {}", state.time_pos, path.display());
            return Ok(());
        }
        ensure!(Instant::now() < timeout,
            "{label}: unexpected state: path {selected:?}, position {}, loaded {}, paused {}, first {}, last {}, next {:?}",
            state.time_pos, state.loaded, state.paused, state.first_frame, state.last_frame, state.next_frame_pos);
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "Requires the synthetic 40-second fixture under the project's local QA folder"]
fn shortcuts_step_through_the_actual_recordings_overlap_handoff() -> Result<()> {
    let qa = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.clipvault-local/qa").canonicalize()?;
    let fixture = PathBuf::from(std::env::var_os("CLIPVAULT_TEST_FIXTURE").context("Set CLIPVAULT_TEST_FIXTURE")?).canonicalize()?;
    ensure!(fixture.starts_with(&qa), "Fixture must be under the project-local QA folder");
    let temp = tempfile::Builder::new().prefix("recording-frame-handoff-").tempdir_in(&qa)?;
    let a = temp.path().join("earlier.mp4");
    let b = temp.path().join("later.mp4");
    let start = 1_768_500_000_000;
    let seam = start + 10_000;
    let mut rows = Vec::new();
    for (number, path, wall_time, from_s, to_s) in [(1, &a, start, 0.0, 20.0), (2, &b, seam, 16.0, 40.0)] {
        cut(
            &[CutSource { path: fixture.clone(), from_s, to_s: Some(to_s) }],
            path, Some(wall_time), &mut |_| {}, &std::sync::atomic::AtomicBool::new(false),
        )?;
        let info = cv_media::probe::probe(path)?;
        rows.push(Recording {
            id: number, path: path.to_string_lossy().into_owned(), start_ms: wall_time,
            duration: info.duration, size: info.size as i64, status: "complete".into(),
            audio_tracks: info.audio.len() as i64, kept: false, probed: true, probe_filetime: None,
        });
    }
    ensure!(rows[0].end_ms() > seam && rows[1].end_ms() > rows[0].end_ms());
    let ctx = egui::Context::default();
    let mut cfg = Config::default();
    cfg.player.volume = 0;
    let mut c = Ctx {
        player: Player::new(ctx.clone(), &cfg),
        service: Service::disconnected_for_test(),
        thumbs: Thumbs::new(ctx.clone()),
        db: Some(Db::open(&temp.path().join("library.db"))?),
        cfg,
        waves: Default::default(),
        accent: theme::TEXT,
        toasts: Vec::new(),
        goto: None,
        open_recording_settings: false,
    };
    c.player.show_mixer = false;
    let mut view = RecordingsView {
        init: true, loaded: Some(Instant::now()), recs: rows, view_end: start + 45_000,
        view_span: 60_000, ..Default::default()
    };
    view.play_at(&mut c, seam, false);
    wait_for(&ctx, &mut view, &mut c, &b, "Later segment, first frame", |state| state.first_frame)?;
    render(&ctx, &mut view, &mut c, Some(egui::Key::Comma));
    wait_for(&ctx, &mut view, &mut c, &a, "Previous frame on earlier segment", |state| {
        !state.last_frame && state.time_pos > 9.95 && state.time_pos < 10.0
            && state.next_frame_pos.is_some_and(|next| (next - 10.0).abs() < 0.000_1)
    })?;
    render(&ctx, &mut view, &mut c, Some(egui::Key::Period));
    wait_for(&ctx, &mut view, &mut c, &b, "Next frame across logical overlap", |state| state.first_frame)?;
    render(&ctx, &mut view, &mut c, Some(egui::Key::Period));
    wait_for(&ctx, &mut view, &mut c, &b, "Later segment, second frame", |state| {
        !state.first_frame && (state.time_pos - 0.04).abs() < 0.000_1
    })
}

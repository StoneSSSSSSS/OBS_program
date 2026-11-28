use super::*;
use cv_core::editor::Rotation;

#[test]
fn paused_cache_limits_retention_and_rejects_in_flight_frames_after_reset() -> Result<()> {
    let ctx = egui::Context::default();
    let mut project = Project::default();
    project.color([0; 3], TICKS_PER_SECOND)?;
    let mut availability = Availability::default();
    let mut preview = Preview::default();
    let mut first = None;

    for tick in 0..=THUMBNAIL_CACHE_SIZE as u64 {
        let pixels = Arc::new(egui::ColorImage::filled(
            [THUMBNAIL_EDGE; 2], Color32::from_gray(tick as u8),
        ));
        if tick == 0 { first = Some(Arc::downgrade(&pixels)); }
        let image = Arc::new(ThumbnailImage { pixels, sources: Some(Vec::new()) });
        let (tx, result) = mpsc::channel();
        tx.send(Ok(image))?;
        preview.thumbnail = Some(ThumbnailTask { key: (1, tick), result, obsolete: false });
        let mut playhead = tick;
        preview.poll(&ctx, &project, 1, &mut playhead, &mut availability);
        assert_eq!(preview.displayed, Some((1, tick)));
        ctx.tex_manager().write().take_delta().clear();
    }
    assert!(first.unwrap().upgrade().is_none());
    assert_eq!(preview.thumbnail_cache.len(), THUMBNAIL_CACHE_SIZE);
    let bytes: usize = preview.thumbnail_cache.iter().map(|(_, image)| {
        image.pixels.pixels.len() * std::mem::size_of::<Color32>()
    }).sum();
    assert_eq!(bytes, 6_553_600);

    let (tx, result) = mpsc::channel();
    preview.thumbnail = Some(ThumbnailTask { key: (1, 0), result, obsolete: false });
    let late = Arc::clone(&preview.thumbnail_cache.back().unwrap().1);
    preview.reset();
    preview.next_thumb = Some(Instant::now() + Duration::from_secs(1));
    tx.send(Ok(late))?;
    preview.poll(&ctx, &project, 1, &mut 0, &mut availability);
    assert!(preview.thumbnail.is_none());
    assert!(preview.thumbnail_cache.is_empty());
    assert!(preview.displayed.is_none());
    assert!(preview.shown.is_none());
    Ok(())
}

fn await_thumbnail(
    preview: &mut Preview, ctx: &egui::Context, project: &Project, revision: u64,
    mut at: u64, availability: &mut Availability,
) -> Result<(Duration, Arc<egui::ColorImage>)> {
    let start = Instant::now();
    while preview.shown != Some((revision, at)) {
        preview.poll(ctx, project, revision, &mut at, availability);
        if preview.shown == Some((revision, at)) { break; }
        anyhow::ensure!(start.elapsed() < Duration::from_secs(15), "Thumbnail timed out");
        std::thread::sleep(Duration::from_micros(250));
    }
    let elapsed = start.elapsed();
    anyhow::ensure!(preview.error.is_none(), "{:?}", preview.error);
    assert_eq!(preview.displayed, Some((revision, at)));
    let texture = preview.texture.as_ref().context("Preview texture missing")?.id();
    let mut deltas = ctx.tex_manager().write().take_delta();
    let upload = deltas.set.remove(&texture).and_then(|mut updates| updates.pop());
    deltas.clear();
    let egui::ImageData::Color(pixels) = upload.context("Preview did not upload the requested frame")?.image;
    Ok((elapsed, pixels))
}

#[test]
#[ignore = "Uses the project-local two-angle Windows editor fixture"]
fn native_repeated_steps_keep_exact_pixels_and_invalidate_on_edit_and_missing_media() -> Result<()> {
    const LEFT: u64 = 17_600_000;
    const RIGHT: u64 = 18_000_000;
    const AFTER: u64 = 18_400_000;
    const STEPS: [u64; 8] = [LEFT, RIGHT, AFTER, RIGHT, LEFT, RIGHT, AFTER, RIGHT];

    let local = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.clipvault-local").canonicalize()?;
    let fixture = local.join("qa/editor-reviews/sports-delivery-two-339d4bfe/output/Center Court Replay.cvproj");
    let work = cv_core::paths::data_dir().canonicalize()?;
    anyhow::ensure!(work.starts_with(&local), "Use an isolated CLIPVAULT_DATA_DIR under .clipvault-local");
    let copies = tempfile::tempdir_in(work)?;
    let mut project = Project::load(&fixture)?;
    for asset in &mut project.assets {
        let copy = copies.path().join(asset.path.file_name().context("Fixture filename missing")?);
        fs::copy(&asset.path, &copy)?;
        asset.path = copy;
    }

    let ctx = egui::Context::default();
    let mut availability = Availability::default();
    availability.sync(&project.assets);
    let mut preview = Preview::default();
    let (cold, _) = await_thumbnail(&mut preview, &ctx, &project, 1, 0, &mut availability)?;
    let mut uncached = Vec::new();
    let mut expected = Vec::new();
    for at in STEPS {
        preview.thumbnail_cache.clear();
        preview.seek(&ctx, &project, 1, at);
        let (elapsed, pixels) = await_thumbnail(&mut preview, &ctx, &project, 1, at, &mut availability)?;
        uncached.push(elapsed);
        expected.push(pixels);
    }
    assert_ne!(expected[0].pixels, expected[1].pixels);
    assert_ne!(expected[1].pixels, expected[2].pixels);

    let mut warmed = Vec::new();
    for at in [LEFT, RIGHT, AFTER] {
        preview.seek(&ctx, &project, 1, at);
        let (_, pixels) = await_thumbnail(&mut preview, &ctx, &project, 1, at, &mut availability)?;
        warmed.push((at, pixels));
    }
    let mut repeated = Vec::new();
    for (at, expected) in STEPS.into_iter().zip(expected) {
        preview.seek(&ctx, &project, 1, at);
        let (elapsed, pixels) = await_thumbnail(&mut preview, &ctx, &project, 1, at, &mut availability)?;
        assert_eq!(*pixels, *expected);
        assert!(Arc::ptr_eq(&pixels, &warmed.iter().find(|(tick, _)| *tick == at).unwrap().1));
        repeated.push(elapsed);
    }
    uncached.sort_unstable();
    repeated.sort_unstable();
    eprintln!("cold {cold:?}, uncached steps median {:?} max {:?}, repeated steps median {:?} max {:?}",
        uncached[4], uncached[7], repeated[4], repeated[7]);

    project.shots[0].visual_transform.rotation = Rotation::Clockwise90;
    preview.invalidate(&ctx);
    assert!(preview.thumbnail_cache.is_empty());
    let (_, changed) = await_thumbnail(&mut preview, &ctx, &project, 2, LEFT, &mut availability)?;
    let newly_rendered = render_thumbnail(&project, LEFT)?;
    assert_eq!(*changed, *newly_rendered.pixels);
    assert_ne!(*changed, *warmed[0].1);

    preview.seek(&ctx, &project, 2, RIGHT);
    await_thumbnail(&mut preview, &ctx, &project, 2, RIGHT, &mut availability)?;
    let missing_asset = &project.assets[0];
    let moved = copies.path().join("Moved court wide.mp4");
    fs::rename(&missing_asset.path, &moved)?;
    assert!(preview.thumbnail_cache.iter().any(|(key, _)| *key == (2, LEFT)));
    preview.seek(&ctx, &project, 2, LEFT);
    let error = await_thumbnail(&mut preview, &ctx, &project, 2, LEFT, &mut availability).unwrap_err();
    assert!(error.to_string().contains("Media offline"), "{error}");
    assert!(availability.missing(missing_asset.id));
    assert_eq!(preview.displayed, Some((2, RIGHT)));
    assert!(preview.thumbnail_cache.iter().all(|(key, _)| *key != (2, LEFT)));

    project.relink(missing_asset.id, moved, missing_asset.info)?;
    availability.sync(&project.assets);
    preview.invalidate(&ctx);
    let (_, recovered) = await_thumbnail(&mut preview, &ctx, &project, 3, LEFT, &mut availability)?;
    assert_eq!(*recovered, *changed);
    assert!(!Arc::ptr_eq(&recovered, &changed));
    Ok(())
}

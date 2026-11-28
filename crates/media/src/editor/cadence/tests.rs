use super::*;
use crate::editor::Apartment;
use std::{fs, path::PathBuf};

fn review_root() -> Result<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.clipvault-local/qa/editor-reviews");
    Ok(root.canonicalize()?)
}

#[test]
#[ignore = "requires the in-project first-time reviewer’s 25 fps export"]
fn changes_only_the_video_timing_table_on_a_half_frame_trim() -> Result<()> {
    let root = review_root()?;
    let original = root.join("client-review-979afd4c/output/Client screening.mp4");
    let before = fs::read(&original)?;
    let work = tempfile::Builder::new().prefix("editor-grid-table-").tempdir_in(&root)?;
    let staged = work.path().join("staged.mp4");
    fs::copy(&original, &staged)?;
    let native = Mp4Reader::open(&staged)?;
    let video = native.video().context("Reviewer fixture has no video")?;
    let (offset, table) = native.video_decode_timing_table(native.tracks[video].id)?
        .context("Reviewer fixture has no video timing table")?;
    drop(native);
    let remuxed = work.path().join("remuxed.mp4");
    let control = RenderControl::default();
    assert!(!normalize(&staged, &remuxed, 25, 145 * TICKS_PER_SECOND / 10, &control)?);
    assert!(!remuxed.exists());

    let after = fs::read(&staged)?;
    let offset = offset as usize;
    let end = offset + table.len();
    assert_eq!(before.len(), after.len());
    assert_eq!(before[..offset], after[..offset]);
    assert_eq!(before[end..], after[end..]);
    assert_ne!(before[offset..end], after[offset..end]);

    // A regular native frame grid bypasses both the in-place patch and remux.
    assert!(!normalize(&staged, &remuxed, 25, 145 * TICKS_PER_SECOND / 10, &control)?);
    assert_eq!(fs::read(&staged)?, after);
    assert_eq!(fs::read(&original)?, before);
    Ok(())
}

#[test]
#[ignore = "requires the in-project first-time reviewer’s duplicate-frame portrait export"]
fn copies_original_aac_samples_and_timestamps_when_reframing_is_necessary() -> Result<()> {
    let root = review_root()?;
    let _apartment = Apartment::enter()?;
    let original = root.join("portrait-review-43b8c4bc/output/courtside-replay.mp4");
    let before = fs::read(&original)?;
    let work = tempfile::Builder::new().prefix("editor-grid-aac-").tempdir_in(&root)?;
    let staged = work.path().join("staged.mp4");
    fs::copy(&original, &staged)?;
    let remuxed = work.path().join("reframed.mp4");
    assert!(normalize(&staged, &remuxed, 30, 15 * TICKS_PER_SECOND, &RenderControl::default())?);
    let native = Mp4Reader::open(&staged)?;
    let fixed = Mp4Reader::open(&remuxed)?;
    let first = native.audio()[0];
    let second = fixed.audio()[0];
    assert_eq!(native.tracks[first].timescale, fixed.tracks[second].timescale);
    let originals = native.samples(first, 0, u64::MAX)?;
    let copies = fixed.samples(second, 0, u64::MAX)?;
    assert_eq!(originals.len(), copies.len());
    for (old, new) in originals.iter().zip(&copies) {
        assert_eq!((old.dts, old.duration, old.cts), (new.dts, new.duration, new.cts));
        assert_eq!(native.read(old)?, fixed.read(new)?);
    }
    assert_eq!(fs::read(&staged)?, before);
    assert_eq!(fs::read(&original)?, before);
    eprintln!("Preserved {} original AAC packets with their exact decode times and durations", copies.len());
    Ok(())
}

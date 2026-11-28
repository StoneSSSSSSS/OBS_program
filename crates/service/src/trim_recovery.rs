//! Durable intent for the single existing file replaced by a trim.

use anyhow::{Context, Result};
use cv_core::db::Db;
use cv_core::model::{AudioEdit, Clip};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::ErrorKind;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::MoveFileW;

type FileVersion = [u32; 7];

pub(crate) fn recovery_dir() -> PathBuf {
    cv_core::paths::data_dir().join("trim-recovery")
}

pub(crate) struct Intent<'a> {
    pub dir: &'a Path,
    pub clip: &'a Clip,
    pub recycle: bool,
}

#[derive(Serialize, Deserialize)]
struct Record {
    token: String,
    clip_id: i64,
    created_at: i64,
    library_path: String,
    previous_start_ms: Option<i64>,
    previous_size: i64,
    previous_duration: f64,
    previous_edits: Vec<AudioEdit>,
    destination: PathBuf,
    stage: PathBuf,
    original: FileVersion,
    replacement: FileVersion,
    backup: FileVersion,
    recycle: bool,
}

pub(crate) struct PendingTrim {
    journal: PathBuf,
    record: Record,
}

impl PendingTrim {
    pub(crate) fn create(intent: Intent<'_>, stage: &Path, destination: &Path) -> Result<Self> {
        let parent = destination
            .parent()
            .context("Trim destination has no parent")?
            .canonicalize()?;
        let destination = parent.join(
            destination
                .file_name()
                .context("Trim destination has no filename")?,
        );
        let stage = stage.canonicalize()?;
        anyhow::ensure!(
            same_path(
                &destination,
                Path::new(&intent.clip.path).canonicalize()?.as_path()
            ),
            "Trim changed its destination"
        );
        anyhow::ensure!(
            stage.parent() == Some(parent.as_path()) && stage != destination,
            "Trim must be staged beside the original"
        );
        let token = cv_core::util::random_token(16);
        let journal = intent.dir.join(format!("{token}.json"));
        let original = version(&destination)?.context("Original clip is missing")?;
        let replacement = version(&stage)?.context("Replacement clip is missing")?;
        anyhow::ensure!(
            original != replacement,
            "Replacement reuses the original clip"
        );
        OpenOptions::new().write(true).open(&stage)?.sync_all()?;
        let backup_path = parent.join(format!(".cv_{token}.original.mp4"));
        if fs::hard_link(&destination, &backup_path).is_err() {
            let mut source = File::open(&destination)?;
            let mut backup = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&backup_path)?;
            std::io::copy(&mut source, &mut backup)?;
            backup.sync_all()?;
        }
        let backup = version(&backup_path)?.context("Failed to preserve the original clip")?;
        anyhow::ensure!(
            version(&destination)? == Some(original),
            "Original clip changed while preserving it"
        );
        let record = Record {
            token,
            clip_id: intent.clip.id,
            created_at: intent.clip.created_at,
            library_path: intent.clip.path.clone(),
            previous_start_ms: intent.clip.source_start_ms,
            previous_size: intent.clip.size,
            previous_duration: intent.clip.duration,
            previous_edits: intent.clip.audio_edits.clone(),
            destination,
            stage,
            original,
            replacement,
            backup,
            recycle: intent.recycle,
        };
        cv_core::util::write_atomic(&journal, &serde_json::to_vec(&record)?)?;
        Ok(Self { journal, record })
    }

    pub(crate) fn key(&self) -> String {
        format!("trim-commit:{}", self.record.token)
    }

    pub(crate) fn publish(&self) -> Result<()> {
        let r = &self.record;
        anyhow::ensure!(
            version(&r.destination)? == Some(r.original),
            "Original clip changed before replacement"
        );
        anyhow::ensure!(
            version(&r.stage)? == Some(r.replacement),
            "Replacement changed while being staged"
        );
        crate::clips::move_file(&r.stage, &r.destination, std::time::Duration::from_secs(2))?;
        Ok(())
    }

    pub(crate) fn resolve(&self, db: &Db) -> Result<()> {
        let r = &self.record;
        let key = self.key();
        let backup = r
            .destination
            .with_file_name(format!(".cv_{}.original.mp4", r.token));
        let hold = r
            .destination
            .with_file_name(format!(".cv_{}.rollback.mp4", r.token));
        db.clip_write(|write| {
            let clip = write
                .clip(r.clip_id)?
                .context("The recorded library entry is missing")?;
            anyhow::ensure!(
                clip.path == r.library_path && clip.created_at == r.created_at,
                "The library clip was replaced or moved"
            );
            let destination_parent = Path::new(&clip.path)
                .parent()
                .context("Invalid library path")?
                .canonicalize()?;
            let real_dest = destination_parent.join(
                Path::new(&clip.path)
                    .file_name()
                    .context("Invalid library file")?,
            );
            anyhow::ensure!(
                same_path(&real_dest, &r.destination)
                    && r.stage.parent() == r.destination.parent()
                    && r.stage != r.destination
                    && r.stage != backup
                    && r.stage != hold,
                "Recovery paths disagree with the library"
            );
            let dest = version(&r.destination)?;
            let saved = version(&backup)?;
            let held = version(&hold)?;
            let staged = version(&r.stage)?;
            anyhow::ensure!(
                saved.is_none_or(|v| v == r.backup)
                    && held.is_none_or(|v| v == r.replacement)
                    && staged.is_none_or(|v| v == r.replacement),
                "An owned recovery path now contains a different file"
            );
            if write.trim_committed(&key)? {
                anyhow::ensure!(
                    dest == Some(r.replacement),
                    "The committed clip has changed on disk"
                );
                if saved.is_some() {
                    if r.recycle {
                        crate::win::shell::recycle(&[backup.as_path()])?;
                    } else {
                        fs::remove_file(&backup)?;
                    }
                }
                let cached_wave = cv_core::waveform::wave_path(Path::new(&r.library_path));
                if let (Some(profile), Some(filename)) = (
                    self.journal.parent().and_then(Path::parent),
                    cached_wave.file_name(),
                ) {
                    let _ = fs::remove_file(profile.join("waves").join(filename));
                }
            } else {
                anyhow::ensure!(
                    clip.source_start_ms == r.previous_start_ms
                        && clip.size == r.previous_size
                        && clip.duration == r.previous_duration
                        && clip.audio_edits == r.previous_edits,
                    "Clip timing, size, or edits changed since the interrupted trim"
                );
                if dest == Some(r.replacement) {
                    anyhow::ensure!(
                        saved == Some(r.backup) && held.is_none(),
                        "Original backup is unavailable or rollback path is occupied"
                    );
                    move_without_replacing(&r.destination, &hold)?;
                    if version(&hold)? != Some(r.replacement) {
                        // A separate process replaced the target during the move.
                        // Restore it only if no newer file has occupied its path.
                        move_without_replacing(&hold, &r.destination)?;
                        anyhow::bail!("Destination changed during recovery");
                    }
                    move_without_replacing(&backup, &r.destination)?;
                } else if dest.is_none() && held == Some(r.replacement) {
                    anyhow::ensure!(saved == Some(r.backup), "Original backup is unavailable");
                    move_without_replacing(&backup, &r.destination)?;
                } else {
                    anyhow::ensure!(
                        dest == Some(r.original) || dest == Some(r.backup),
                        "The destination has a different file or has been removed"
                    );
                    if saved.is_some() {
                        fs::remove_file(&backup)?;
                    }
                }
            }
            for path in [&r.stage, &hold] {
                if version(path)? == Some(r.replacement) {
                    fs::remove_file(path)?;
                }
            }
            Ok(())
        })?;
        fs::remove_file(&self.journal)?;
        db.clear_trim_commit(&key)?;
        Ok(())
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    crate::shared::same_path(&a.to_string_lossy(), &b.to_string_lossy())
}

fn move_without_replacing(from: &Path, to: &Path) -> Result<()> {
    let from: Vec<_> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<_> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe { MoveFileW(PCWSTR(from.as_ptr()), PCWSTR(to.as_ptr())) }?;
    Ok(())
}

fn version(path: &Path) -> Result<Option<FileVersion>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            metadata.file_type().is_file(),
            "Expected an ordinary recovery file: {}",
            path.display()
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    Ok(Some(crate::jobs::file_version(path)?))
}

pub(crate) fn recover_on_start() {
    let dir = recovery_dir();
    if !crate::shared::media_path_allowed(&dir) || !dir.is_dir() {
        return;
    }
    if let Err(error) = Db::open_default().and_then(|db| recover_pending(&db, &dir, crate::shared::require_media_path)) {
        tracing::warn!("Could not finish interrupted trims: {error:#}");
    }
}

fn recover_pending(db: &Db, dir: &Path, check_path: impl Fn(&Path) -> Result<()>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let journal = entry?.path();
        if journal.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let result = (|| -> Result<()> {
            check_path(&journal)?;
            let record: Record = serde_json::from_slice(&fs::read(&journal)?)?;
            anyhow::ensure!(
                record.token.len() == 32
                    && record.token.bytes().all(|c| c.is_ascii_hexdigit())
                    && journal.file_stem().and_then(|stem| stem.to_str()) == Some(&record.token),
                "Invalid trim operation identity"
            );
            check_path(&record.destination)?;
            check_path(&record.stage)?;
            PendingTrim {
                journal: journal.clone(),
                record,
            }
            .resolve(db)
        })();
        if let Err(error) = result {
            tracing::warn!(
                "Interrupted trim needs attention ({}): {error:#}",
                journal.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn test_crash(destination: &Path, at: &str) {
    let Ok(dir) = std::env::var("CLIPVAULT_TEST_TRIM_CHILD") else {
        return;
    };
    let Some(parent) = destination.parent() else {
        return;
    };
    if parent != Path::new(&dir) || std::env::var("CLIPVAULT_TEST_TRIM_PHASE").as_deref() != Ok(at)
    {
        return;
    }
    std::process::exit(if at == "before-commit" { 71 } else { 72 });
}

#[cfg(test)]
mod tests {
    use super::*;
    use cv_core::model::NewClip;
    use std::process::Command;
    use std::sync::atomic::AtomicBool;

    const ORIGINAL: &[u8] = b"original synthetic media";
    const TRIMMED: &[u8] = b"trimmed synthetic media";
    const FOREIGN: &[u8] = b"newer unrelated contents";
    const NAME: &str = "owned.mp4";

    #[test]
    #[ignore = "Only run by the process interruption test with a synthetic workspace"]
    fn trim_crash_child() -> Result<()> {
        let dir = PathBuf::from(std::env::var("CLIPVAULT_TEST_TRIM_CHILD")?);
        anyhow::ensure!(
            dir.is_absolute() && dir.join("synthetic-test-only").is_file(),
            "Missing isolated child marker"
        );
        let journal = dir.join("trim-recovery");
        let original_path = dir.join(NAME);
        let temp_path = dir.join("stage.mp4");
        let db = Db::open(&dir.join("library.db"))?;
        let clip = db.clip(1)?.context("synthetic test clip")?;
        crate::jobs::commit_clip_output_inner(
            &db,
            &temp_path,
            &AtomicBool::new(false),
            Some(Intent {
                dir: &journal,
                clip: &clip,
                recycle: false,
            }),
            |write| {
                write.trim(clip.id, Some(5000), &[])?;
                write.probe(clip.id, 1.0, TRIMMED.len() as i64, 320, 192, 25.0, &[])?;
                Ok((clip.id, original_path.clone(), true))
            },
        )?;
        anyhow::bail!("Expected abrupt process termination");
    }

    fn interrupt(at: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().canonicalize().unwrap();
        fs::write(p.join("synthetic-test-only"), b"only artificial test files").unwrap();
        fs::write(p.join(NAME), ORIGINAL).unwrap();
        fs::write(p.join("stage.mp4"), TRIMMED).unwrap();
        let db = Db::open(&p.join("library.db")).unwrap();
        let id = db
            .insert_clip(&NewClip {
                path: p.join(NAME).to_string_lossy().into_owned(),
                title: "Legacy synthetic clip".into(),
                game_id: None,
                created_at: 100,
                source_start_ms: None,
                size: ORIGINAL.len() as i64,
                source: "import".into(),
                favorite: false,
            })
            .unwrap();
        assert_eq!(id, 1);
        db.update_clip_probe(id, 2.0, ORIGINAL.len() as i64, 320, 192, 25.0, 0, &[])
            .unwrap();
        // The child opens a pre-audio-edits library and exercises migration.
        db.conn()
            .execute("ALTER TABLE clips DROP COLUMN audio_edits", [])
            .unwrap();
        drop(db);
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "trim_recovery::tests::trim_crash_child",
                "--exact",
                "--ignored",
            ])
            .env("CLIPVAULT_TEST_TRIM_CHILD", &p)
            .env("CLIPVAULT_TEST_TRIM_PHASE", at)
            .output()
            .unwrap();
        assert_eq!(
            status.status.code(),
            Some(if at == "before-commit" { 71 } else { 72 }),
            "child stdout: {}; stderr: {}",
            String::from_utf8_lossy(&status.stdout),
            String::from_utf8_lossy(&status.stderr)
        );
        dir
    }

    #[test]
    fn abrupt_precommit_exit_exposes_mismatch_then_restores_original() {
        let dir = interrupt("before-commit");
        let db = Db::open(&dir.path().join("library.db")).unwrap();
        let journal = dir.path().join("trim-recovery");
        assert_eq!(db.clip(1).unwrap().unwrap().duration, 2.0);
        assert_eq!(fs::read(dir.path().join(NAME)).unwrap(), TRIMMED);
        let unrelated = dir.path().join(".cv_unowned.mp4");
        fs::write(&unrelated, FOREIGN).unwrap();
        for _ in 0..2 {
            recover_pending(&db, &journal, |_| Ok(())).unwrap();
            let clip = db.clip(1).unwrap().unwrap();
            assert_eq!(clip.duration, 2.0);
            assert_eq!(clip.source_start_ms, None);
            assert_eq!(fs::read(dir.path().join(NAME)).unwrap(), ORIGINAL);
            assert_eq!(fs::read(&unrelated).unwrap(), FOREIGN);
            assert_eq!(fs::read_dir(&journal).unwrap().count(), 0);
        }
    }

    #[test]
    fn abrupt_postcommit_exit_keeps_committed_clip() {
        let dir = interrupt("after-commit");
        let db = Db::open(&dir.path().join("library.db")).unwrap();
        let journal = dir.path().join("trim-recovery");
        for _ in 0..2 {
            recover_pending(&db, &journal, |_| Ok(())).unwrap();
            let clip = db.clip(1).unwrap().unwrap();
            assert_eq!(clip.duration, 1.0);
            assert_eq!(clip.source_start_ms, Some(5000));
            assert_eq!(fs::read(dir.path().join(NAME)).unwrap(), TRIMMED);
            assert_eq!(fs::read_dir(&journal).unwrap().count(), 0);
        }
        assert!(!fs::read_dir(dir.path()).unwrap().flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .ends_with(".original.mp4")
        }));
    }

    #[test]
    fn recovery_preserves_newer_destination_and_conflicting_files() {
        let dir = interrupt("before-commit");
        let db = Db::open(&dir.path().join("library.db")).unwrap();
        let journal = dir.path().join("trim-recovery");
        let journal_path = fs::read_dir(&journal)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let record: Record = serde_json::from_slice(&fs::read(journal_path).unwrap()).unwrap();
        let original = dir
            .path()
            .join(format!(".cv_{}.original.mp4", record.token));
        let hold = dir
            .path()
            .join(format!(".cv_{}.rollback.mp4", record.token));
        fs::write(&hold, FOREIGN).unwrap();
        recover_pending(&db, &journal, |_| Ok(())).unwrap();
        assert_eq!(fs::read(&hold).unwrap(), FOREIGN);
        assert_eq!(fs::read(dir.path().join(NAME)).unwrap(), TRIMMED);
        fs::remove_file(&hold).unwrap();
        db.set_clip_audio_edits(1, &[AudioEdit::new("mic", 0.1, 0.4, 0.0)])
            .unwrap();
        recover_pending(&db, &journal, |_| Ok(())).unwrap();
        assert_eq!(fs::read(dir.path().join(NAME)).unwrap(), TRIMMED);
        assert_eq!(db.clip(1).unwrap().unwrap().audio_edits.len(), 1);
        db.set_clip_audio_edits(1, &[]).unwrap();
        fs::write(dir.path().join(NAME), FOREIGN).unwrap();
        recover_pending(&db, &journal, |_| Ok(())).unwrap();
        assert_eq!(fs::read(dir.path().join(NAME)).unwrap(), FOREIGN);
        assert_eq!(fs::read(original).unwrap(), ORIGINAL);
        assert_eq!(db.clip(1).unwrap().unwrap().duration, 2.0);
        assert_eq!(fs::read_dir(&journal).unwrap().count(), 1);
    }
}

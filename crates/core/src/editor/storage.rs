use super::Project;
use anyhow::{ensure, Context, Result};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const MAX_PROJECT_BYTES: u64 = 32 * 1024 * 1024;

fn project_path(path: &Path) -> Result<PathBuf> {
    ensure!(path.extension().and_then(|s| s.to_str()).is_some_and(|s| s.eq_ignore_ascii_case("cvproj")),
        "Use the .cvproj extension for an Editor project.");
    Ok(std::path::absolute(path)?)
}

fn read_project(path: &Path) -> Result<Project> {
    let file = std::fs::File::open(path).with_context(|| format!("Open {}", path.display()))?;
    ensure!(file.metadata()?.is_file(), "Choose an Editor project file.");
    let mut bytes = Vec::new();
    file.take(MAX_PROJECT_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= MAX_PROJECT_BYTES, "This project exceeds the 32 MB document limit.");
    let project: Project = serde_json::from_slice(&bytes).context("This file is not a readable ClipVault Editor project.")?;
    project.validate()?;
    Ok(project)
}

fn portable_path(path: &Path, folder: &Path) -> PathBuf {
    if let Ok(relative) = path.strip_prefix(folder) {
        return relative.to_owned();
    }
    // Keep sibling Media/ and Projects/ directories movable together. Outside media keeps its full path.
    if let Some(parent) = folder.parent().filter(|p| p.parent().is_some()) {
        if let Ok(relative) = path.strip_prefix(parent) {
            return Path::new("..").join(relative);
        }
    }
    path.to_owned()
}

impl Project {
    pub fn load(path: &Path) -> Result<Self> {
        let path = project_path(path)?;
        let folder = path.parent().context("Missing project folder.")?;
        let mut project = read_project(&path)?;
        for asset in &mut project.assets {
            if asset.path.is_relative() {
                asset.path = std::path::absolute(folder.join(&asset.path))?;
            }
            // Resolve existing files once; missing files keep a usable path for relinking.
            if let Ok(path) = std::fs::canonicalize(&asset.path) {
                // Keep ordinary Windows paths for shell dialogs and system media APIs.
                #[cfg(windows)]
                {
                    let text = path.to_string_lossy();
                    asset.path = if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
                        PathBuf::from(format!(r"\\{unc}"))
                    } else if let Some(drive) = text.strip_prefix(r"\\?\") {
                        PathBuf::from(drive)
                    } else { path };
                }
                #[cfg(not(windows))]
                { asset.path = path; }
            }
        }
        Ok(project)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let path = project_path(path)?;
        let folder = path.parent().context("Missing project folder.")?;
        let existing = path.try_exists()?;
        if existing {
            read_project(&path).context("The destination is already occupied. Choose another project name.")?;
        }
        let mut document = self.clone();
        for asset in &mut document.assets {
            ensure!(asset.path != path, "Choose a project destination separate from the source media.");
            asset.path = portable_path(&asset.path, folder);
        }
        let mut temp = tempfile::NamedTempFile::new_in(folder).context("Create a temporary project file.")?;
        serde_json::to_writer_pretty(&mut temp, &document)?;
        writeln!(temp)?;
        ensure!(temp.as_file().metadata()?.len() <= MAX_PROJECT_BYTES, "The project exceeds the 32 MB document limit.");
        temp.as_file().sync_all()?;
        if existing {
            temp.persist(&path).context("Replace the saved project.")?;
        } else {
            temp.persist_noclobber(&path).context("Another file appeared at that destination. Choose another name.")?;
        }
        Ok(())
    }
}

//! ClipVault profile, executable resources and Windows integration paths.

use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};

const PROJECT_MARKER: &str = ".clipvault-project";
const LOCAL_ROOT: &str = ".clipvault-local";
const DATA_ENV: &str = "CLIPVAULT_DATA_DIR";

#[derive(Debug)]
struct Storage {
    profile: PathBuf,
    // Only executable-selected local profiles have an independent anchor.
    anchor: Option<PathBuf>,
    local: bool,
}

impl Storage {
    fn resolve(exe: &Path, override_dir: Option<&OsStr>, local_app_data: Option<&Path>) -> Self {
        if let Some(directory) = override_dir {
            let path = PathBuf::from(directory);
            let profile = if directory.to_string_lossy().trim().is_empty() {
                PathBuf::new()
            } else if path.is_absolute() || path.has_root() || matches!(path.components().next(), Some(Component::Prefix(_))) {
                path
            } else {
                exe.join(path)
            };
            return Self { profile, anchor: None, local: true };
        }

        if let Some(base) = local_app_data.filter(|base| base.is_absolute()) {
            let conventional = base.join("Programs").join(crate::APP_NAME);
            let components = |path: &Path| {
                path.components().map(|component| component.as_os_str().to_string_lossy().to_lowercase()).collect::<Vec<_>>()
            };
            if components(exe) == components(&conventional) {
                return Self { profile: base.join(crate::APP_NAME), anchor: None, local: false };
            }
        }

        let anchor = exe.ancestors().find(|path| path.join(PROJECT_MARKER).is_file())
            .or_else(|| {
                // A packaged app can be moved together with its local folder
                // without carrying the source checkout's marker.
                let local = exe.parent().filter(|path| path.file_name() == Some(OsStr::new(LOCAL_ROOT)))?;
                if exe.file_name() == Some(OsStr::new("app")) { local.parent() } else { None }
            })
            .unwrap_or(exe);
        Self { profile: anchor.join(LOCAL_ROOT).join("profile"), anchor: Some(anchor.into()), local: true }
    }

    fn default_media_folder(&self, name: &str, user: Option<&Path>) -> PathBuf {
        if let Some(home) = user.filter(|home| !self.local && home.is_absolute()) {
            home.join("Videos").join(crate::APP_NAME).join(name)
        } else {
            self.profile.join(name.to_ascii_lowercase())
        }
    }

    fn instance_name(&self, component: &str) -> String {
        if self.local {
            let canonical = resolved_path(&self.profile).unwrap_or_else(|_| self.profile.clone());
            // Stable across launchers built with different Rust versions.
            let hash = canonical.to_string_lossy().to_lowercase().bytes().fold(
                0xcbf2_9ce4_8422_2325_u64,
                |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3),
            );
            format!("Local\\ClipVault{component}-{hash:016x}")
        } else {
            format!("Local\\ClipVault{component}")
        }
    }
}

fn storage() -> Storage {
    let override_dir = std::env::var_os(DATA_ENV);
    let local_app_data = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    Storage::resolve(&exe_dir(), override_dir.as_deref(), local_app_data.as_deref())
}

pub fn data_dir() -> PathBuf {
    storage().profile
}

/// Portable/local profiles use only profile-relative mutable defaults.
pub fn uses_local_storage() -> bool {
    storage().local
}

pub fn default_media_folder(name: &str) -> PathBuf {
    let user = std::env::var_os("USERPROFILE").map(PathBuf::from);
    storage().default_media_folder(name, user.as_deref())
}

/// Identify separate installations without sharing Windows named objects.
pub fn instance_name(component: &str) -> String {
    storage().instance_name(component)
}

fn resolved_path(path: &Path) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "profile path must be absolute"));
    }
    if !path.components().any(|component| matches!(component, Component::ParentDir)) {
        match std::fs::canonicalize(path) {
            Ok(path) => return Ok(path),
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            Err(_) => {}
        }
        // A dangling or inaccessible junction is not an ordinary missing path.
        match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
            Ok(_) => return Err(io::Error::new(io::ErrorKind::NotFound, "path cannot be resolved")),
        }
        if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
            return Ok(resolved_path(parent)?.join(name));
        }
    }
    Err(io::Error::new(io::ErrorKind::InvalidInput, "path cannot be safely resolved"))
}

fn contains_resolved(parent: &Path, path: &Path) -> bool {
    match (resolved_path(parent), resolved_path(path)) {
        (Ok(parent), Ok(path)) => path.starts_with(parent),
        _ => false,
    }
}

/// True for paths that resolve inside the selected local profile. An existing
/// junction or symlink cannot redirect a child outside the profile.
pub fn owns_local_path(path: &Path) -> bool {
    let layout = storage();
    layout.local && contains_resolved(&layout.profile, path)
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.toml")
}

pub fn db_path() -> PathBuf {
    data_dir().join("library.db")
}

pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

pub fn thumbs_dir() -> PathBuf {
    data_dir().join("thumbs")
}

pub fn temp_dir() -> PathBuf {
    data_dir().join("tmp")
}

/// Written by the service; tells the GUI which port/token to use.
pub fn ipc_info_path() -> PathBuf {
    data_dir().join("ipc.json")
}

pub fn ensure_dirs() -> io::Result<()> {
    ensure_storage_dirs(&storage())
}

fn ensure_storage_dirs(layout: &Storage) -> io::Result<()> {
    let profile = &layout.profile;
    if !profile.is_absolute() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!(
            "ClipVault cannot use storage at {}: choose an absolute profile path",
            profile.display()
        )));
    }
    if layout.anchor.as_ref().is_some_and(|anchor| !contains_resolved(anchor, profile)) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!(
            "ClipVault profile {} resolves outside its application folder or cannot be accessed",
            profile.display()
        )));
    }
    let mut directories = vec![profile.clone(), profile.join("logs"), profile.join("thumbs"), profile.join("tmp")];
    if layout.local {
        directories.extend(["recordings", "clips", "exports"].map(|name| profile.join(name)));
    }
    for d in directories {
        if layout.local && !contains_resolved(profile, &d) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!(
                "ClipVault cannot safely use storage at {}", d.display()
            )));
        }
        std::fs::create_dir_all(&d).map_err(|error| io::Error::new(error.kind(), format!(
            "ClipVault cannot create storage at {}: {error}", d.display()
        )))?;
        if layout.local {
            let probe = d.join(format!(".write-check-{}", crate::util::random_token(8)));
            let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&probe)
                .map_err(|error| io::Error::new(error.kind(), format!(
                    "ClipVault cannot write to {}: {error}", d.display()
                )))?;
            drop(file);
            std::fs::remove_file(probe)?;
        }
    }
    Ok(())
}

/// Directory containing the running executable.
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Look for a resource shipped next to the executable (or in `assets/` beside it,
/// or — during development — in the workspace `assets/` folder).
pub fn find_resource(name: &str) -> Option<PathBuf> {
    let exe = exe_dir();
    let mut candidates = vec![exe.join(name), exe.join("assets").join(name)];
    // target/{debug,release}/ -> workspace root
    if let Some(root) = exe.parent().and_then(Path::parent) {
        candidates.push(root.join("assets").join(name));
        candidates.push(root.join("third_party").join(name));
    }
    candidates.into_iter().find(|p| p.is_file())
}

/// OBS's configuration directory (`%APPDATA%\obs-studio` unless overridden).
pub fn obs_config_dir(override_dir: &str) -> PathBuf {
    if !override_dir.trim().is_empty() {
        return PathBuf::from(override_dir.trim());
    }
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("obs-studio")
}

/// The user's Startup folder (for the "start with Windows" shortcut).
pub fn startup_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| {
        PathBuf::from(a)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup")
    })
}

pub fn start_menu_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| {
        PathBuf::from(a)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
    })
}

pub const SERVICE_EXE: &str = "clipvault-service.exe";
pub const GUI_EXE: &str = "clipvault.exe";
pub const STARTUP_LINK_NAME: &str = "ClipVault Service.lnk";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marked_project_shares_one_profile_between_debug_and_packaged_builds() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("ClipVault Project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(PROJECT_MARKER), b"ClipVault project root").unwrap();
        let debug = Storage::resolve(&project.join("target/debug/build/test/out"), None, None);
        let package = Storage::resolve(&project.join(".clipvault-local/app"), None, None);
        assert!(debug.local && package.local);
        assert_eq!(debug.profile, project.join(".clipvault-local/profile"));
        assert_eq!(debug.profile, package.profile);
        let name = debug.instance_name("Service");
        assert_eq!(name, package.instance_name("Service"));
        ensure_storage_dirs(&debug).unwrap();
        assert_eq!(name, debug.instance_name("Service"));
        for name in ["recordings", "clips", "exports", "logs", "thumbs", "tmp"] {
            assert!(debug.profile.join(name).is_dir());
        }
    }

    #[test]
    fn standalone_release_copies_do_not_share_media_or_mutex_names() {
        let temp = tempfile::tempdir().unwrap();
        let first = Storage::resolve(&temp.path().join("first"), None, None);
        let second = Storage::resolve(&temp.path().join("second"), None, None);
        assert!(first.local && second.local);
        assert_eq!(first.profile, temp.path().join("first/.clipvault-local/profile"));
        assert_ne!(first.profile, second.profile);
        assert_ne!(first.instance_name("Gui"), second.instance_name("Gui"));
        assert_ne!(first.instance_name("Gui"), first.instance_name("Service"));
        assert_eq!(first.default_media_folder("Recordings", None), first.profile.join("recordings"));
        let packaged = Storage::resolve(&temp.path().join("first/.clipvault-local/app"), None, None);
        assert_eq!(packaged.profile, first.profile);
    }

    #[test]
    fn explicit_profile_overrides_legacy_and_anchors_relative_paths_to_the_exe() {
        let temp = tempfile::tempdir().unwrap();
        let resources = temp.path().join("Local");
        let installed = resources.join("Programs/ClipVault");
        let profile = temp.path().join("Explicit Data");
        let user = temp.path().join("User");
        let layout = Storage::resolve(&installed, Some(profile.as_os_str()), Some(&resources));
        assert!(layout.local);
        assert_eq!(layout.profile, profile);
        for (name, folder) in [("Recordings", "recordings"), ("Clips", "clips"), ("Exports", "exports")] {
            assert_eq!(layout.default_media_folder(name, Some(&user)), profile.join(folder));
        }
        let relative = Storage::resolve(&installed, Some(OsStr::new("smoke-profile")), Some(&resources));
        assert_eq!(relative.profile, installed.join("smoke-profile"));
        let blank = Storage::resolve(&installed, Some(OsStr::new(" ")), Some(&resources));
        assert!(blank.local);
        assert!(ensure_storage_dirs(&blank).is_err());
    }

    #[test]
    fn conventional_install_retains_legacy_paths_without_opening_them() {
        let temp = tempfile::tempdir().unwrap();
        let resources = temp.path().join("Local");
        let user = temp.path().join("User");
        let installed = Storage::resolve(&resources.join("Programs/CLIPVAULT"), None, Some(&resources));
        assert!(!installed.local);
        assert_eq!(installed.profile, resources.join("ClipVault"));
        assert_eq!(installed.default_media_folder("Clips", Some(&user)), user.join("Videos/ClipVault/Clips"));
        assert_eq!(installed.instance_name("Service"), r"Local\ClipVaultService");
        assert!(!installed.profile.exists());
        assert!(!user.exists());
    }

    #[test]
    fn unusable_profiles_fail_and_containment_checks_missing_children() {
        let temp = tempfile::tempdir().unwrap();
        let blocked = temp.path().join("file-profile");
        std::fs::write(&blocked, b"existing file").unwrap();
        let profile = Storage::resolve(temp.path(), Some(blocked.as_os_str()), None);
        assert!(ensure_storage_dirs(&profile).is_err());
        assert!(blocked.is_file());

        let root = temp.path().join("data");
        assert!(contains_resolved(&root, &root.join("clips/new.mp4")));
        assert!(!contains_resolved(&root, &temp.path().join("data-sibling/new.mp4")));
        assert!(!contains_resolved(&root, &root.join("../new.mp4")));
    }

    #[cfg(windows)]
    #[test]
    fn containment_rejects_a_symlink_to_another_directory() {
        use std::os::windows::fs::symlink_dir;

        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("profile");
        let outside = temp.path().join("other");
        std::fs::create_dir(&parent).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let link = parent.join("escape");
        match symlink_dir(&outside, &link) {
            Ok(()) => assert!(!contains_resolved(&parent, &link.join("new.mp4"))),
            Err(error) if error.raw_os_error() == Some(1314) => {
                // Developer Mode or SeCreateSymbolicLinkPrivilege is optional.
                eprintln!("Skipping symlink assertion: {error}");
            }
            Err(error) => panic!("create test symlink: {error}"),
        }
    }
}

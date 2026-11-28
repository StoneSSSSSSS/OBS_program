//! User configuration (`%LOCALAPPDATA%\ClipVault\config.toml`).
//!
//! Every field has a default so that old config files keep loading after new
//! settings are added, and unknown keys (like the old `[obs]` section) are
//! ignored. `Config::sanitize` clamps values into safe ranges; the GUI and
//! service both call it after loading.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub recording: RecordingConfig,
    pub clips: ClipsConfig,
    pub hotkeys: HotkeyConfig,
    pub overlay: OverlayConfig,
    pub audio: AudioConfig,
    pub games: GamesConfig,
    pub player: PlayerConfig,
    pub export: ExportConfig,
    pub advanced: AdvancedConfig,
}

/// `%USERPROFILE%\Videos\ClipVault\<sub>`: where recordings and clips go
/// unless the user picks somewhere else.
pub fn default_media_folder(sub: &str) -> String {
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:\\"));
    home.join("Videos").join(crate::APP_NAME).join(sub).to_string_lossy().to_string()
}

// ---------------------------------------------------------------- general

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Accent {
    Mono,
    #[default]
    Red,
    Blue,
    Green,
    Purple,
    Orange,
}

impl Accent {
    pub const ALL: [Accent; 6] = [Accent::Mono, Accent::Red, Accent::Blue, Accent::Green, Accent::Purple, Accent::Orange];
    pub fn label(self) -> &'static str {
        match self {
            Accent::Mono => "Monochrome",
            Accent::Red => "Red",
            Accent::Blue => "Blue",
            Accent::Green => "Green",
            Accent::Purple => "Purple",
            Accent::Orange => "Orange",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct GeneralConfig {
    /// Start the background recorder when you log in.
    pub start_with_windows: bool,
    /// Show the ClipVault icon in the notification area.
    pub show_tray_icon: bool,
    /// Show Windows notifications for problems (mic lost, recording stopped, low disk).
    pub notifications: bool,
    /// Accent colour used sparingly on top of the greyscale theme.
    pub accent: Accent,
    /// Interface zoom (1.0 = 100%).
    pub ui_scale: f32,
    /// Ask before deleting clips.
    pub confirm_deletes: bool,
    /// Which page the app opens on: "home", "recordings", "clips".
    pub start_page: String,
    /// Minimise animations in the app.
    pub reduce_motion: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self { start_with_windows: true, show_tray_icon: true, notifications: true, accent: Accent::Red, ui_scale: 1.0, confirm_deletes: true, start_page: "home".into(), reduce_motion: false }
    }
}

// -------------------------------------------------------------- recording

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RecordingConfig {
    /// Keep a continuous 24/7 recording. When off, only a short rolling
    /// buffer is kept so the clip hotkey still works.
    pub enabled: bool,
    pub folder: String,
    /// Split the continuous recording into files of this many minutes.
    pub segment_minutes: u32,
    /// Keep this many hours of 24/7 footage (time the PC is off doesn't count).
    pub retention_hours: u32,
    /// Additionally delete footage older than this many days (0 = never).
    pub max_age_days: u32,
    /// Also cap total size of recordings (GB). 0 = no cap.
    pub max_storage_gb: u32,
    /// Always keep at least this much free space on the recordings drive (GB).
    pub min_free_space_gb: u32,
    /// Frames per second.
    pub fps: u32,
    /// Picture height cap ("1080" = 1080p); 0 = the monitor's full resolution.
    pub max_height: u32,
    /// "low", "medium", "high", "very_high" or "custom".
    pub quality: String,
    /// Video bitrate when `quality` is "custom".
    pub bitrate_kbps: u32,
    /// "auto" (GPU encoder, else software) or "software".
    pub encoder: String,
    /// Keyframe every N seconds (smaller = snappier scrubbing and exact cuts).
    pub keyframe_interval_secs: u32,
    /// "primary" or a monitor name like `\\.\DISPLAY2`.
    pub monitor: String,
    /// "auto", "duplication" or "graphics_capture".
    pub capture_method: String,
    /// Draw the mouse cursor into the video.
    pub show_cursor: bool,
    /// Bitrate of the full-mix and microphone tracks.
    pub audio_bitrate_kbps: u32,
    /// Record each app's sound on its own track (for the expanded audio view).
    pub app_tracks: bool,
    /// How many apps can have their own track at once.
    pub app_track_count: u32,
    /// Bitrate of each app track.
    pub app_bitrate_kbps: u32,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            folder: default_media_folder("Recordings"),
            segment_minutes: 30,
            retention_hours: 48,
            max_age_days: 0,
            max_storage_gb: 0,
            min_free_space_gb: 30,
            fps: 60,
            max_height: 1080,
            quality: "high".into(),
            bitrate_kbps: 15_000,
            encoder: "auto".into(),
            keyframe_interval_secs: 1,
            monitor: "primary".into(),
            capture_method: "auto".into(),
            show_cursor: true,
            audio_bitrate_kbps: 160,
            app_tracks: true,
            app_track_count: 8,
            app_bitrate_kbps: 128,
        }
    }
}

pub const QUALITIES: &[(&str, &str)] = &[("low", "Low (small files)"), ("medium", "Medium"), ("high", "High (recommended)"), ("very_high", "Very high (big files)"), ("custom", "Custom bitrate")];
pub const HEIGHTS: &[(u32, &str)] = &[(0, "Full resolution"), (1440, "1440p"), (1080, "1080p (recommended)"), (720, "720p")];

impl RecordingConfig {
    /// Video bitrate (kbit/s) for a picture of this size.
    pub fn video_kbps(&self, w: u32, h: u32) -> u32 {
        let bpp = match self.quality.as_str() {
            "low" => 0.045,
            "medium" => 0.07,
            "very_high" => 0.16,
            "custom" => return self.bitrate_kbps,
            _ => 0.10,
        };
        ((w as f64 * h as f64 * self.fps as f64 * bpp / 1000.0) as u32).clamp(1_500, 150_000)
    }
}

// ------------------------------------------------------------------ clips

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ClipsConfig {
    pub folder: String,
    /// How many seconds the clip hotkey saves.
    pub clip_length_secs: u32,
    /// Put clips into one sub-folder per game.
    pub organize_by_game: bool,
    /// File name template. Tokens: {game} {date} {time} {exe}.
    pub name_template: String,
    /// Name used when no game/app can be identified.
    pub unknown_game_name: String,
    /// Seconds of activity history used to work out which game a clip is from.
    pub attribution_window_secs: u32,
    /// Ignore a second clip request within this many ms (double presses).
    pub save_cooldown_ms: u32,
    pub generate_thumbnails: bool,
    /// Pick up video files you copy into the clips folder yourself.
    pub import_untracked_files: bool,
    /// Send deleted clips to the Recycle Bin instead of erasing them.
    pub delete_to_recycle_bin: bool,
    /// Length of the "short clip" hotkey.
    pub short_clip_secs: u32,
    /// Mark clips saved while a game is running as favourites automatically.
    pub auto_favorite: bool,
}

impl Default for ClipsConfig {
    fn default() -> Self {
        Self {
            folder: default_media_folder("Clips"),
            clip_length_secs: 120,
            organize_by_game: true,
            name_template: "{game} {date} {time}".into(),
            unknown_game_name: "Desktop".into(),
            attribution_window_secs: 60,
            save_cooldown_ms: 1500,
            generate_thumbnails: true,
            import_untracked_files: true,
            delete_to_recycle_bin: true,
            short_clip_secs: 30,
            auto_favorite: false,
        }
    }
}

// ---------------------------------------------------------------- hotkeys

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HotkeyConfig {
    /// Save the last `clips.clip_length_secs` as a clip.
    pub save_clip: String,
    /// Save only the last `clips.short_clip_secs` seconds.
    pub save_short_clip: String,
    /// Drop a bookmark on the 24/7 timeline without saving a clip.
    pub add_marker: String,
    /// Pause/resume the 24/7 recording.
    pub toggle_recording: String,
    /// Open the ClipVault window.
    pub open_gui: String,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self { save_clip: "Ctrl+Shift+`".into(), save_short_clip: String::new(), add_marker: "Ctrl+Shift+M".into(), toggle_recording: String::new(), open_gui: String::new() }
    }
}

// ---------------------------------------------------------------- overlay

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct OverlayConfig {
    /// Show your overlay image when a clip is saved.
    pub enabled: bool,
    /// Empty = the bundled overlay_img.png.
    pub image_path: String,
    pub duration_ms: u32,
    /// "top_right", "top_left", "top_center", "bottom_right", "bottom_left".
    pub position: String,
    pub margin_px: u32,
    pub opacity: f32,
    pub scale: f32,
    /// "primary" or "active" (the monitor your game is on).
    pub monitor: String,
    /// Small text notices (marker added, clip failed, mic switched).
    pub show_text_toasts: bool,
    pub play_sound: bool,
    /// Empty = Windows notification sound. Otherwise a .wav file.
    pub sound_path: String,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            image_path: String::new(),
            duration_ms: 5000,
            position: "top_right".into(),
            margin_px: 10,
            opacity: 1.0,
            scale: 1.0,
            monitor: "primary".into(),
            show_text_toasts: true,
            play_sound: false,
            sound_path: String::new(),
        }
    }
}

// ------------------------------------------------------------------ audio

/// Track names of the legacy OBS layout (old recordings).
pub const TRACK_NAMES: [&str; 6] = ["Full mix", "Game", "Voice chat", "Music", "Microphone", "Desktop"];

pub const DEFAULT_VOICE_APPS: &[&str] = &["discord.exe", "discordptb.exe", "discordcanary.exe", "teamspeak.exe", "ts3client_win64.exe", "teams.exe", "ms-teams.exe", "zoom.exe", "skype.exe", "slack.exe", "mumble.exe", "guilded.exe"];
pub const DEFAULT_MEDIA_APPS: &[&str] = &[
    "spotify.exe", "chrome.exe", "msedge.exe", "firefox.exe", "brave.exe", "opera.exe", "opera_gx.exe", "vivaldi.exe", "vlc.exe", "mpv.exe", "wmplayer.exe", "music.ui.exe", "video.ui.exe", "applemusic.exe", "itunes.exe", "foobar2000.exe", "aimp.exe", "tidal.exe", "deezer.exe", "youtube music.exe", "potplayermini64.exe", "mpc-hc64.exe",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AudioConfig {
    /// Microphones in order of preference (matched by name, so a headset that
    /// Windows re-registers as "2- Headset" is still found).
    pub mic_priority: Vec<String>,
    /// When no preferred mic is connected: "windows_default" or "none".
    pub mic_fallback: String,
    /// Watch audio devices and keep recording the right mic.
    pub mic_guard: bool,
    /// Record the microphone at all.
    pub record_mic: bool,
    /// Mic volume multiplier (1.0 = unchanged).
    pub mic_gain: f32,
    /// Warn when the mic has been silent while recording for this many minutes (0 = never).
    pub mic_silence_warning_mins: u32,
    /// Apps in the "Voice chat" category.
    pub voice_apps: Vec<String>,
    /// Apps in the "Music & media" category (music players and browsers).
    pub music_apps: Vec<String>,
    /// Your own category for an app (exe -> "game", "voice", "media", "other").
    pub app_categories: BTreeMap<String, String>,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            mic_priority: Vec::new(),
            mic_fallback: "windows_default".into(),
            mic_guard: true,
            record_mic: true,
            mic_gain: 1.0,
            mic_silence_warning_mins: 0,
            voice_apps: DEFAULT_VOICE_APPS.iter().map(|s| s.to_string()).collect(),
            music_apps: DEFAULT_MEDIA_APPS.iter().map(|s| s.to_string()).collect(),
            app_categories: BTreeMap::new(),
        }
    }
}

impl AudioConfig {
    /// The category an app belongs to by the user's lists (None = decide
    /// automatically, e.g. game detection).
    pub fn category_of(&self, exe: &str) -> Option<&'static str> {
        let e = exe.to_lowercase();
        if let Some(c) = self.app_categories.get(&e) {
            return Some(crate::mixer::normalize_category(c));
        }
        if self.voice_apps.iter().any(|a| a.eq_ignore_ascii_case(&e)) {
            return Some("voice");
        }
        if self.music_apps.iter().any(|a| a.eq_ignore_ascii_case(&e)) {
            return Some("media");
        }
        None
    }
}

// ------------------------------------------------------------------ games

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct GamesConfig {
    /// Executables always treated as games (e.g. "javaw.exe").
    pub always_games: Vec<String>,
    /// Executables never treated as games.
    pub never_games: Vec<String>,
    /// Treat any full-screen app (that isn't a browser/tool) as a game.
    pub fullscreen_is_game: bool,
    /// Extra folders that contain games (anything run from here is a game).
    pub library_folders: Vec<String>,
}

impl Default for GamesConfig {
    fn default() -> Self {
        Self { always_games: Vec::new(), never_games: Vec::new(), fullscreen_is_game: true, library_folders: Vec::new() }
    }
}

// ----------------------------------------------------------------- player

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PlayerConfig {
    /// 0-100.
    pub volume: u32,
    pub autoplay: bool,
    pub loop_clips: bool,
    pub seek_small_secs: u32,
    pub seek_large_secs: u32,
    /// "full" (play the full mix) or "separate" (play categories/apps separately).
    pub default_mix_mode: String,
    /// How many seconds behind real time "Go live" starts.
    pub live_offset_secs: u32,
    /// Hours visible on the 24/7 timeline by default.
    pub timeline_hours: u32,
    pub show_activity_on_timeline: bool,
    /// Remember the mixer per clip.
    pub remember_clip_mix: bool,
    pub default_speed: f32,
    /// Fade volume in/out at the edges of audio edits instead of switching instantly.
    pub smooth_edits: bool,
    /// Length of those fades.
    pub edit_fade_ms: u32,
    /// Audio lanes you've hidden (lane ids, e.g. "cat:other").
    pub hidden_lanes: Vec<String>,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            volume: 80,
            autoplay: true,
            loop_clips: false,
            seek_small_secs: 5,
            seek_large_secs: 30,
            default_mix_mode: "separate".into(),
            live_offset_secs: 6,
            timeline_hours: 6,
            show_activity_on_timeline: true,
            remember_clip_mix: true,
            default_speed: 1.0,
            smooth_edits: true,
            edit_fade_ms: 30,
            hidden_lanes: Vec::new(),
        }
    }
}

impl PlayerConfig {
    /// Ramp length (seconds) for edits.
    pub fn fade_s(&self) -> f64 {
        if self.smooth_edits {
            self.edit_fade_ms as f64 / 1000.0
        } else {
            0.0
        }
    }
}

// ----------------------------------------------------------------- export

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ExportConfig {
    /// Empty = `<clips folder>\_Exports`.
    pub folder: String,
    pub audio_bitrate_kbps: u32,
    /// Default size target for "Share" exports, in MB.
    pub target_size_mb: u32,
    /// Size presets offered next to Share (Discord: 10 / 50 / 500 MB).
    pub size_presets_mb: Vec<u32>,
    /// Max height for size-targeted exports (0 = keep).
    pub max_height: u32,
    /// Copy the exported file to the clipboard so you can paste it into Discord.
    pub copy_to_clipboard: bool,
    pub open_folder_after: bool,
}

impl Default for ExportConfig {
    fn default() -> Self {
        Self { folder: String::new(), audio_bitrate_kbps: 160, target_size_mb: 10, size_presets_mb: vec![10, 50, 500], max_height: 1080, copy_to_clipboard: true, open_folder_after: false }
    }
}

// --------------------------------------------------------------- advanced

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AdvancedConfig {
    pub ipc_port: u16,
    /// "error", "warn", "info", "debug", "trace".
    pub log_level: String,
    pub log_retention_days: u32,
    pub activity_sample_ms: u32,
    pub health_check_secs: u32,
    /// Keep game activity history for this many days beyond the recordings.
    pub activity_history_days: u32,
}

impl Default for AdvancedConfig {
    fn default() -> Self {
        Self { ipc_port: 47821, log_level: "info".into(), log_retention_days: 14, activity_sample_ms: 1000, health_check_secs: 3, activity_history_days: 30 }
    }
}

// ------------------------------------------------------------- load/save

impl Config {
    /// Load from `path`; a missing file yields defaults. Values are sanitised.
    pub fn load_from(path: &Path) -> anyhow::Result<Config> {
        let mut cfg = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str::<Config>(&text).map_err(|e| anyhow::anyhow!("config {} is invalid: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e.into()),
        };
        cfg.sanitize();
        Ok(cfg)
    }

    pub fn load() -> anyhow::Result<Config> {
        Self::load_from(&crate::paths::config_path())
    }

    /// Load, falling back to defaults (and keeping a backup of a broken file).
    pub fn load_or_default() -> Config {
        let path = crate::paths::config_path();
        match Self::load_from(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("{e:#}; using defaults");
                let backup = path.with_extension("toml.broken");
                let _ = std::fs::copy(&path, backup);
                Config::default()
            }
        }
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        let text = toml::to_string_pretty(self)?;
        let header = "# ClipVault settings. Edit in the app (Settings page) or here.\n\
                      # Unknown keys are ignored; missing keys use defaults.\n\n";
        crate::util::write_atomic(path, format!("{header}{text}").as_bytes())?;
        Ok(())
    }

    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&crate::paths::config_path())
    }

    /// Clamp everything into sane ranges.
    pub fn sanitize(&mut self) {
        fn clamp<T: PartialOrd>(v: &mut T, lo: T, hi: T) {
            if *v < lo {
                *v = lo;
            } else if *v > hi {
                *v = hi;
            }
        }
        let g = &mut self.general;
        if !g.ui_scale.is_finite() {
            g.ui_scale = 1.0;
        }
        clamp(&mut g.ui_scale, 0.6, 2.5);
        if !["home", "recordings", "clips"].contains(&g.start_page.as_str()) {
            g.start_page = "home".into();
        }

        let r = &mut self.recording;
        clamp(&mut r.segment_minutes, 1, 240);
        clamp(&mut r.retention_hours, 1, 24 * 60);
        clamp(&mut r.max_age_days, 0, 3650);
        clamp(&mut r.min_free_space_gb, 1, 10_000);
        clamp(&mut r.fps, 10, 240);
        clamp(&mut r.max_height, 0, 4320);
        clamp(&mut r.bitrate_kbps, 500, 300_000);
        clamp(&mut r.keyframe_interval_secs, 1, 10);
        clamp(&mut r.audio_bitrate_kbps, 64, 320);
        clamp(&mut r.app_bitrate_kbps, 64, 320);
        clamp(&mut r.app_track_count, 1, 24);
        if !QUALITIES.iter().any(|(k, _)| *k == r.quality) {
            r.quality = "high".into();
        }
        if !["auto", "software"].contains(&r.encoder.as_str()) {
            r.encoder = "auto".into();
        }
        if !["auto", "duplication", "graphics_capture"].contains(&r.capture_method.as_str()) {
            r.capture_method = "auto".into();
        }
        if r.monitor.trim().is_empty() {
            r.monitor = "primary".into();
        }
        if r.folder.trim().is_empty() {
            r.folder = default_media_folder("Recordings");
        }

        let c = &mut self.clips;
        clamp(&mut c.clip_length_secs, 5, 3600);
        clamp(&mut c.attribution_window_secs, 5, 3600);
        clamp(&mut c.save_cooldown_ms, 0, 10_000);
        clamp(&mut c.short_clip_secs, 5, 600);
        if c.name_template.trim().is_empty() {
            c.name_template = "{game} {date} {time}".into();
        }
        if c.unknown_game_name.trim().is_empty() {
            c.unknown_game_name = "Desktop".into();
        }
        if c.folder.trim().is_empty() {
            c.folder = default_media_folder("Clips");
        }

        let ov = &mut self.overlay;
        clamp(&mut ov.duration_ms, 500, 30_000);
        clamp(&mut ov.margin_px, 0, 500);
        if !ov.opacity.is_finite() {
            ov.opacity = 1.0;
        }
        clamp(&mut ov.opacity, 0.1, 1.0);
        if !ov.scale.is_finite() {
            ov.scale = 1.0;
        }
        clamp(&mut ov.scale, 0.2, 4.0);
        if !["top_right", "top_left", "top_center", "bottom_right", "bottom_left"].contains(&ov.position.as_str()) {
            ov.position = "top_right".into();
        }
        if !["primary", "active"].contains(&ov.monitor.as_str()) {
            ov.monitor = "primary".into();
        }

        let a = &mut self.audio;
        if !["windows_default", "none"].contains(&a.mic_fallback.as_str()) {
            a.mic_fallback = "windows_default".into();
        }
        if !a.mic_gain.is_finite() {
            a.mic_gain = 1.0;
        }
        clamp(&mut a.mic_gain, 0.0, 8.0);
        clamp(&mut a.mic_silence_warning_mins, 0, 600);
        dedup_case_insensitive(&mut a.mic_priority);
        dedup_case_insensitive(&mut a.voice_apps);
        dedup_case_insensitive(&mut a.music_apps);
        a.app_categories = std::mem::take(&mut a.app_categories)
            .into_iter()
            .filter(|(k, _)| !k.trim().is_empty())
            .map(|(k, v)| (k.trim().to_lowercase(), crate::mixer::normalize_category(&v).to_string()))
            .collect();

        let gm = &mut self.games;
        dedup_case_insensitive(&mut gm.always_games);
        dedup_case_insensitive(&mut gm.never_games);

        let p = &mut self.player;
        clamp(&mut p.volume, 0, 100);
        clamp(&mut p.seek_small_secs, 1, 60);
        clamp(&mut p.seek_large_secs, 5, 600);
        clamp(&mut p.live_offset_secs, 2, 120);
        clamp(&mut p.timeline_hours, 1, 24 * 14);
        clamp(&mut p.edit_fade_ms, 1, 1000);
        if !p.default_speed.is_finite() {
            p.default_speed = 1.0;
        }
        clamp(&mut p.default_speed, 0.1, 4.0);
        if !["full", "separate"].contains(&p.default_mix_mode.as_str()) {
            p.default_mix_mode = "separate".into();
        }
        dedup_case_insensitive(&mut p.hidden_lanes);

        let e = &mut self.export;
        clamp(&mut e.audio_bitrate_kbps, 64, 320);
        clamp(&mut e.target_size_mb, 1, 4096);
        clamp(&mut e.max_height, 0, 4320);
        e.size_presets_mb.retain(|m| (1..=4096).contains(m));
        e.size_presets_mb.sort_unstable();
        e.size_presets_mb.dedup();
        if e.size_presets_mb.is_empty() {
            e.size_presets_mb = vec![10, 50, 500];
        }

        let ad = &mut self.advanced;
        if ad.ipc_port < 1024 {
            ad.ipc_port = 47821;
        }
        clamp(&mut ad.log_retention_days, 1, 365);
        clamp(&mut ad.activity_sample_ms, 250, 10_000);
        clamp(&mut ad.health_check_secs, 1, 60);
        clamp(&mut ad.activity_history_days, 1, 3650);
        if !["error", "warn", "info", "debug", "trace"].contains(&ad.log_level.as_str()) {
            ad.log_level = "info".into();
        }
    }

    pub fn exports_dir(&self) -> PathBuf {
        if self.export.folder.trim().is_empty() {
            PathBuf::from(&self.clips.folder).join("_Exports")
        } else {
            PathBuf::from(self.export.folder.trim())
        }
    }

    /// Settings that need the recorder restarted to take effect.
    pub fn recorder_restart_needed(&self, other: &Config) -> bool {
        let (a, b) = (&self.recording, &other.recording);
        a.enabled != b.enabled
            || a.folder != b.folder
            || a.segment_minutes != b.segment_minutes
            || a.fps != b.fps
            || a.max_height != b.max_height
            || a.quality != b.quality
            || a.bitrate_kbps != b.bitrate_kbps
            || a.encoder != b.encoder
            || a.keyframe_interval_secs != b.keyframe_interval_secs
            || a.monitor != b.monitor
            || a.capture_method != b.capture_method
            || a.show_cursor != b.show_cursor
            || a.audio_bitrate_kbps != b.audio_bitrate_kbps
            || a.app_tracks != b.app_tracks
            || a.app_track_count != b.app_track_count
            || a.app_bitrate_kbps != b.app_bitrate_kbps
            || self.audio.record_mic != other.audio.record_mic
    }

    /// If the recordings/clips folders are on a drive that doesn't exist on
    /// this PC (e.g. a settings file copied from another computer), use the
    /// default folders instead. Returns true if anything changed.
    pub fn fix_missing_drives(&mut self) -> bool {
        fn drive_missing(p: &str) -> bool {
            let path = Path::new(p);
            match path.components().next() {
                Some(std::path::Component::Prefix(pre)) => !Path::new(&format!("{}\\", pre.as_os_str().to_string_lossy())).exists(),
                _ => false,
            }
        }
        let mut changed = false;
        if drive_missing(&self.recording.folder) {
            tracing::warn!("recordings folder {} is on a missing drive; using the default", self.recording.folder);
            self.recording.folder = default_media_folder("Recordings");
            changed = true;
        }
        if drive_missing(&self.clips.folder) {
            tracing::warn!("clips folder {} is on a missing drive; using the default", self.clips.folder);
            self.clips.folder = default_media_folder("Clips");
            changed = true;
        }
        if !self.export.folder.is_empty() && drive_missing(&self.export.folder) {
            self.export.folder.clear();
            changed = true;
        }
        changed
    }
}

fn dedup_case_insensitive(v: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    v.retain(|s| {
        let t = s.trim();
        !t.is_empty() && seen.insert(t.to_lowercase())
    });
    for s in v.iter_mut() {
        *s = s.trim().to_string();
    }
}

/// Values recovered from the old Python program's `configs.txt`.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LegacySettings {
    pub hotkey: Option<String>,
    pub recordings: Option<String>,
    pub clips: Option<String>,
    pub keep_recordings: Option<u32>,
}

/// Parse the legacy `key=value` file. Tolerates missing/extra lines.
pub fn parse_legacy_configs(text: &str) -> LegacySettings {
    let mut out = LegacySettings::default();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let k = k.trim().to_lowercase();
        let v = v.trim().to_string();
        if v.is_empty() {
            continue;
        }
        match k.as_str() {
            "overlay hotkey" => out.hotkey = Some(legacy_hotkey_to_ours(&v)),
            "recordings path" => out.recordings = Some(v),
            "clips path" => out.clips = Some(v),
            "number of recordings before deleting" => out.keep_recordings = v.parse().ok(),
            _ => {}
        }
    }
    out
}

/// "ctrl+shift+`" -> "Ctrl+Shift+`"
fn legacy_hotkey_to_ours(s: &str) -> String {
    s.split('+')
        .map(|p| {
            let p = p.trim();
            let mut c = p.chars();
            match c.next() {
                Some(f) if p.len() > 1 => f.to_uppercase().collect::<String>() + c.as_str(),
                _ => p.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

impl Config {
    /// Fold legacy settings into this config. `segment_minutes_legacy` is the
    /// old split length, used to convert "keep N recordings" into hours.
    pub fn apply_legacy(&mut self, legacy: &LegacySettings, segment_minutes_legacy: u32) {
        if let Some(h) = &legacy.hotkey {
            self.hotkeys.save_clip = h.clone();
        }
        if let Some(p) = &legacy.recordings {
            self.recording.folder = p.clone();
        }
        if let Some(p) = &legacy.clips {
            self.clips.folder = p.clone();
        }
        if let Some(n) = legacy.keep_recordings {
            let minutes = n.max(1) * segment_minutes_legacy.max(1);
            self.recording.retention_hours = minutes.div_ceil(60).max(1);
        }
        self.sanitize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_roundtrip_through_toml() {
        let cfg = Config::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn old_files_load_and_ignore_obs_keys() {
        let text = r#"
            [obs]
            exe_path = "C:\\obs64.exe"
            [recording]
            retention_hours = 24
            encoder = "jim_nvenc"
            format = "fragmented_mp4"
            some_future_key = "x"
            [audio]
            voice_apps = ["Discord.exe", "discord.exe", "  TeamSpeak.exe "]
        "#;
        let mut cfg: Config = toml::from_str(text).unwrap();
        cfg.sanitize();
        assert_eq!(cfg.recording.retention_hours, 24);
        assert_eq!(cfg.recording.encoder, "auto", "old OBS encoder ids map to auto");
        assert_eq!(cfg.recording.segment_minutes, 30);
        assert_eq!(cfg.audio.voice_apps, vec!["Discord.exe", "TeamSpeak.exe"]);
        assert_eq!(cfg.hotkeys.save_clip, "Ctrl+Shift+`");
    }

    #[test]
    fn sanitize_clamps() {
        let mut cfg = Config::default();
        cfg.recording.fps = 0;
        cfg.recording.segment_minutes = 0;
        cfg.general.ui_scale = f32::NAN;
        cfg.overlay.position = "middle".into();
        cfg.recording.quality = "ultra".into();
        cfg.export.size_presets_mb = vec![0, 50, 10, 50];
        cfg.audio.app_categories.insert(" Chrome.EXE ".into(), "music".into());
        cfg.sanitize();
        assert_eq!(cfg.recording.fps, 10);
        assert_eq!(cfg.recording.segment_minutes, 1);
        assert_eq!(cfg.general.ui_scale, 1.0);
        assert_eq!(cfg.overlay.position, "top_right");
        assert_eq!(cfg.recording.quality, "high");
        assert_eq!(cfg.export.size_presets_mb, vec![10, 50]);
        assert_eq!(cfg.audio.app_categories.get("chrome.exe").map(String::as_str), Some("media"));
    }

    #[test]
    fn categories_and_bitrates() {
        let mut a = AudioConfig::default();
        assert_eq!(a.category_of("Discord.exe"), Some("voice"));
        assert_eq!(a.category_of("chrome.exe"), Some("media"));
        assert_eq!(a.category_of("cs2.exe"), None);
        a.app_categories.insert("chrome.exe".into(), "other".into());
        assert_eq!(a.category_of("CHROME.exe"), Some("other"));
        let r = RecordingConfig::default();
        let k = r.video_kbps(1920, 1080);
        assert!((10_000..16_000).contains(&k), "{k}");
        let c = RecordingConfig { quality: "custom".into(), bitrate_kbps: 7000, ..Default::default() };
        assert_eq!(c.video_kbps(1920, 1080), 7000);
    }

    #[test]
    fn load_missing_file_gives_defaults_and_save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        let cfg = Config::load_from(&p).unwrap();
        assert_eq!(cfg, Config::default());
        let mut cfg2 = cfg.clone();
        cfg2.clips.clip_length_secs = 420;
        cfg2.audio.mic_priority = vec!["Headset Microphone (Astro A50 Voice)".into()];
        cfg2.save_to(&p).unwrap();
        let back = Config::load_from(&p).unwrap();
        assert_eq!(back, cfg2);
    }

    #[test]
    fn invalid_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "this is = = not toml").unwrap();
        assert!(Config::load_from(&p).is_err());
    }

    #[test]
    fn legacy_configs_are_migrated() {
        let text = "overlay img name=overlay_img.png\noverlay hotkey=ctrl+shift+`\n\
                    recordings path=I:\\OBS\\Recordings\nclips path=I:\\OBS\\Clips\n\
                    number of recordings before deleting=48";
        let legacy = parse_legacy_configs(text);
        assert_eq!(legacy.hotkey.as_deref(), Some("Ctrl+Shift+`"));
        assert_eq!(legacy.keep_recordings, Some(48));
        let mut cfg = Config::default();
        cfg.apply_legacy(&legacy, 60);
        assert_eq!(cfg.recording.retention_hours, 48);
        assert_eq!(cfg.recording.folder, r"I:\OBS\Recordings");
        assert_eq!(cfg.clips.folder, r"I:\OBS\Clips");
    }

    #[test]
    fn missing_drives_fall_back_to_defaults() {
        let mut cfg = Config::default();
        cfg.recording.folder = r"Q:\Nope\Recordings".into();
        cfg.clips.folder = r"C:\Somewhere\Clips".into();
        assert!(cfg.fix_missing_drives());
        assert_eq!(cfg.recording.folder, default_media_folder("Recordings"));
        assert_eq!(cfg.clips.folder, r"C:\Somewhere\Clips");
        assert!(!cfg.fix_missing_drives());
    }

    #[test]
    fn restart_detection() {
        let a = Config::default();
        let mut b = a.clone();
        b.player.volume = 10;
        assert!(!a.recorder_restart_needed(&b));
        b.recording.fps = 30;
        assert!(a.recorder_restart_needed(&b));
    }
}

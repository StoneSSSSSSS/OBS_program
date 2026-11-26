//! User configuration (`%LOCALAPPDATA%\ClipVault\config.toml`).
//!
//! Every field has a default so that old config files keep loading after new
//! settings are added, and unknown keys are ignored. `Config::sanitize` clamps
//! values into safe ranges; the GUI and service both call it after loading.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub obs: ObsConfig,
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

impl Default for Config {
    fn default() -> Self {
        Self {
            general: GeneralConfig::default(),
            obs: ObsConfig::default(),
            recording: RecordingConfig::default(),
            clips: ClipsConfig::default(),
            hotkeys: HotkeyConfig::default(),
            overlay: OverlayConfig::default(),
            audio: AudioConfig::default(),
            games: GamesConfig::default(),
            player: PlayerConfig::default(),
            export: ExportConfig::default(),
            advanced: AdvancedConfig::default(),
        }
    }
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
    pub const ALL: [Accent; 6] = [
        Accent::Mono,
        Accent::Red,
        Accent::Blue,
        Accent::Green,
        Accent::Purple,
        Accent::Orange,
    ];
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
    /// Start the background service when you log in.
    pub start_with_windows: bool,
    /// Show the ClipVault icon in the notification area.
    pub show_tray_icon: bool,
    /// Show Windows notifications for problems (mic lost, OBS crashed, low disk).
    pub notifications: bool,
    /// Accent colour used sparingly on top of the greyscale theme.
    pub accent: Accent,
    /// Interface zoom (1.0 = 100%).
    pub ui_scale: f32,
    /// Ask before deleting clips.
    pub confirm_deletes: bool,
    /// Which page the GUI opens on: "home", "recordings", "clips".
    pub start_page: String,
    /// Minimise animations in the GUI.
    pub reduce_motion: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            start_with_windows: true,
            show_tray_icon: true,
            notifications: true,
            accent: Accent::Red,
            ui_scale: 1.0,
            confirm_deletes: true,
            start_page: "home".into(),
            reduce_motion: false,
        }
    }
}

// -------------------------------------------------------------------- obs

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ObsConfig {
    pub exe_path: String,
    /// Empty = `%APPDATA%\obs-studio`.
    pub config_dir: String,
    /// Launch OBS when the service starts.
    pub launch_on_start: bool,
    /// Seconds to wait after login before launching OBS.
    pub launch_delay_secs: u32,
    pub minimize_to_tray: bool,
    /// Relaunch OBS automatically if it crashes.
    pub restart_on_crash: bool,
    /// Relaunch OBS even if you close it yourself.
    pub restart_if_closed: bool,
    /// Give up auto-restarting after this many restarts in an hour.
    pub max_restarts_per_hour: u32,
    pub websocket_port: u16,
    /// Empty = read OBS's own websocket password from its config.
    pub websocket_password: String,
    /// Let ClipVault write OBS output settings (format, tracks, encoder...).
    pub manage_settings: bool,
    /// OBS process priority: "normal", "above_normal", "high".
    pub process_priority: String,
    /// Seconds to wait for OBS to close gracefully before ending it.
    pub shutdown_timeout_secs: u32,
}

impl Default for ObsConfig {
    fn default() -> Self {
        Self {
            exe_path: r"C:\Program Files\obs-studio\bin\64bit\obs64.exe".into(),
            config_dir: String::new(),
            launch_on_start: true,
            launch_delay_secs: 0,
            minimize_to_tray: true,
            restart_on_crash: true,
            restart_if_closed: false,
            max_restarts_per_hour: 10,
            websocket_port: 4455,
            websocket_password: String::new(),
            manage_settings: true,
            process_priority: "above_normal".into(),
            shutdown_timeout_secs: 20,
        }
    }
}

// -------------------------------------------------------------- recording

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RecordingConfig {
    /// Keep a continuous 24/7 recording running.
    pub enabled: bool,
    pub folder: String,
    /// Split the continuous recording into files of this many minutes.
    pub segment_minutes: u32,
    /// Keep this many hours of 24/7 footage (newest first). Time the PC is
    /// off doesn't count, just like the old "keep N recordings" setting.
    pub retention_hours: u32,
    /// Additionally delete footage older than this many days (0 = never).
    pub max_age_days: u32,
    /// Also cap total size of recordings (GB). 0 = no cap.
    pub max_storage_gb: u32,
    /// Always keep at least this much free space on the recordings drive (GB).
    pub min_free_space_gb: u32,
    /// "fragmented_mp4" (crash-safe), "hybrid_mp4", "mkv".
    pub format: String,
    /// OBS encoder id: jim_nvenc, jim_hevc_nvenc, jim_av1_nvenc, obs_x264, ...
    pub encoder: String,
    /// "CQP", "CBR", "VBR".
    pub rate_control: String,
    /// Constant quality level for CQP (lower = better quality, bigger files).
    pub cqp: u32,
    pub bitrate_kbps: u32,
    pub max_bitrate_kbps: u32,
    /// NVENC preset p1 (fastest) .. p7 (best quality); x264: ultrafast..slow.
    pub preset: String,
    /// Keyframe every N seconds (smaller = more precise fast cuts).
    pub keyframe_interval_secs: u32,
    /// Frames per second.
    pub fps: u32,
    /// Output resolution "WIDTHxHEIGHT"; empty = same as canvas.
    pub output_resolution: String,
    /// Bitrate for each audio track.
    pub audio_bitrate_kbps: u32,
    /// Restart the recording if OBS stops it unexpectedly.
    pub auto_restart: bool,
    /// Treat the recording as stuck if the file stops growing for this long.
    pub stall_timeout_secs: u32,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            folder: r"I:\OBS\Recordings".into(),
            segment_minutes: 30,
            retention_hours: 48,
            max_age_days: 0,
            max_storage_gb: 0,
            min_free_space_gb: 50,
            format: "fragmented_mp4".into(),
            encoder: "jim_nvenc".into(),
            rate_control: "CQP".into(),
            cqp: 25,
            bitrate_kbps: 20_000,
            max_bitrate_kbps: 30_000,
            preset: "p2".into(),
            keyframe_interval_secs: 2,
            fps: 60,
            output_resolution: String::new(),
            audio_bitrate_kbps: 160,
            auto_restart: true,
            stall_timeout_secs: 90,
        }
    }
}

pub const RECORDING_FORMATS: &[(&str, &str)] = &[
    ("fragmented_mp4", "Fragmented MP4 (crash-safe, recommended)"),
    ("hybrid_mp4", "Hybrid MP4 (crash-safe, finalised on stop)"),
    ("mkv", "Matroska MKV (crash-safe)"),
];

pub const ENCODERS: &[(&str, &str)] = &[
    ("jim_nvenc", "NVIDIA NVENC H.264"),
    ("jim_hevc_nvenc", "NVIDIA NVENC HEVC (H.265)"),
    ("jim_av1_nvenc", "NVIDIA NVENC AV1 (RTX 40+)"),
    ("obs_x264", "x264 (CPU)"),
];

// ------------------------------------------------------------------ clips

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ClipsConfig {
    pub folder: String,
    /// Default clip length = how many seconds the replay buffer keeps.
    pub clip_length_secs: u32,
    /// Replay buffer memory cap (MB).
    pub replay_max_memory_mb: u32,
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
    /// Length of the "short clip" hotkey (trimmed from the replay).
    pub short_clip_secs: u32,
    /// Mark clips saved while a game is running as favourites automatically.
    pub auto_favorite: bool,
}

impl Default for ClipsConfig {
    fn default() -> Self {
        Self {
            folder: r"I:\OBS\Clips".into(),
            clip_length_secs: 120,
            replay_max_memory_mb: 8192,
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
    /// Save the replay buffer as a clip.
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
        Self {
            save_clip: "Ctrl+Shift+`".into(),
            save_short_clip: String::new(),
            add_marker: "Ctrl+Shift+M".into(),
            toggle_recording: String::new(),
            open_gui: String::new(),
        }
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

/// The fixed multi-track layout ClipVault configures in OBS.
pub const TRACK_NAMES: [&str; 6] = ["Full mix", "Game", "Voice chat", "Music", "Microphone", "Desktop"];
pub const TRACK_FULL: usize = 1;
pub const TRACK_GAME: usize = 2;
pub const TRACK_VOICE: usize = 3;
pub const TRACK_MUSIC: usize = 4;
pub const TRACK_MIC: usize = 5;
pub const TRACK_DESKTOP: usize = 6;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AudioConfig {
    /// Record 6 separate audio tracks (full mix + game/voice/music/mic/desktop).
    pub multitrack: bool,
    /// Microphones in order of preference (matched by name, so a headset that
    /// Windows re-registers as "2- Headset" is still found).
    pub mic_priority: Vec<String>,
    /// When no preferred mic is connected: "windows_default" or "none".
    pub mic_fallback: String,
    /// Watch audio devices and keep OBS pointed at the right mic.
    pub mic_guard: bool,
    /// "default" (follow Windows) or a device name for desktop audio.
    pub desktop_device: String,
    /// Apps recorded on the "Voice chat" track.
    pub voice_apps: Vec<String>,
    /// Apps recorded on the "Music" track.
    pub music_apps: Vec<String>,
    /// Point the "Game" track at whatever game you are playing.
    pub game_audio_capture: bool,
    /// If a voice/music app plays on a different device than desktop audio,
    /// add it to the full mix so it isn't lost.
    pub auto_mix_other_devices: bool,
    /// Add OBS's noise suppression to the mic.
    pub mic_noise_suppression: bool,
    /// Mic volume multiplier applied in OBS (1.0 = unchanged).
    pub mic_gain: f32,
    /// Warn when the mic has been silent while recording for this many minutes (0 = never).
    pub mic_silence_warning_mins: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            multitrack: true,
            mic_priority: Vec::new(),
            mic_fallback: "windows_default".into(),
            mic_guard: true,
            desktop_device: "default".into(),
            voice_apps: vec!["Discord.exe".into()],
            music_apps: vec!["Spotify.exe".into()],
            game_audio_capture: true,
            auto_mix_other_devices: true,
            mic_noise_suppression: false,
            mic_gain: 1.0,
            mic_silence_warning_mins: 0,
        }
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
    /// A game must be focused this long before game audio follows it.
    pub audio_follow_delay_secs: u32,
}

impl Default for GamesConfig {
    fn default() -> Self {
        Self {
            always_games: Vec::new(),
            never_games: Vec::new(),
            fullscreen_is_game: true,
            library_folders: Vec::new(),
            audio_follow_delay_secs: 3,
        }
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
    /// mpv hardware decoding mode: "auto-safe", "auto-copy-safe", "no".
    pub hwdec: String,
    /// "full" (play the full mix) or "separate" (play individual tracks).
    pub default_mix_mode: String,
    /// How many seconds behind real time "Go live" starts.
    pub live_offset_secs: u32,
    /// Hours visible on the 24/7 timeline by default.
    pub timeline_hours: u32,
    pub show_activity_on_timeline: bool,
    /// Remember the mixer per clip.
    pub remember_clip_mix: bool,
    /// Playback speed choices shown in the player.
    pub default_speed: f32,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            volume: 80,
            autoplay: true,
            loop_clips: false,
            seek_small_secs: 5,
            seek_large_secs: 30,
            hwdec: "auto-safe".into(),
            default_mix_mode: "full".into(),
            live_offset_secs: 6,
            timeline_hours: 6,
            show_activity_on_timeline: true,
            remember_clip_mix: true,
            default_speed: 1.0,
        }
    }
}

// ----------------------------------------------------------------- export

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ExportConfig {
    /// Empty = `<clips folder>\_Exports`.
    pub folder: String,
    /// Frame-exact cuts (re-encodes; slower). Off = instant keyframe cuts.
    pub precise_cuts: bool,
    /// ffmpeg encoder for re-encodes: "h264_nvenc", "hevc_nvenc", "libx264".
    pub video_encoder: String,
    /// Quality for re-encodes (CQ/CRF, lower = better).
    pub quality: u32,
    pub audio_bitrate_kbps: u32,
    /// Size target for the "Share (small)" export, in MB.
    pub target_size_mb: u32,
    /// Max height for size-targeted exports (0 = keep).
    pub max_height: u32,
    /// Copy the exported file to the clipboard so you can paste it into Discord.
    pub copy_to_clipboard: bool,
    pub open_folder_after: bool,
}

impl Default for ExportConfig {
    fn default() -> Self {
        Self {
            folder: String::new(),
            precise_cuts: false,
            video_encoder: "h264_nvenc".into(),
            quality: 23,
            audio_bitrate_kbps: 192,
            target_size_mb: 10,
            max_height: 1080,
            copy_to_clipboard: true,
            open_folder_after: false,
        }
    }
}

// --------------------------------------------------------------- advanced

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AdvancedConfig {
    /// Empty = auto-detect.
    pub ffmpeg_path: String,
    pub ffprobe_path: String,
    pub mpv_dll_path: String,
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
        Self {
            ffmpeg_path: String::new(),
            ffprobe_path: String::new(),
            mpv_dll_path: String::new(),
            ipc_port: 47821,
            log_level: "info".into(),
            log_retention_days: 14,
            activity_sample_ms: 1000,
            health_check_secs: 3,
            activity_history_days: 30,
        }
    }
}

// ------------------------------------------------------------- load/save

impl Config {
    /// Load from `path`; a missing file yields defaults. Values are sanitised.
    pub fn load_from(path: &Path) -> anyhow::Result<Config> {
        let mut cfg = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str::<Config>(&text)
                .map_err(|e| anyhow::anyhow!("config {} is invalid: {e}", path.display()))?,
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

        let o = &mut self.obs;
        clamp(&mut o.launch_delay_secs, 0, 600);
        clamp(&mut o.max_restarts_per_hour, 1, 100);
        clamp(&mut o.shutdown_timeout_secs, 3, 120);
        if o.websocket_port == 0 {
            o.websocket_port = 4455;
        }
        if !["normal", "above_normal", "high"].contains(&o.process_priority.as_str()) {
            o.process_priority = "above_normal".into();
        }

        let r = &mut self.recording;
        clamp(&mut r.segment_minutes, 1, 240);
        clamp(&mut r.retention_hours, 1, 24 * 60);
        clamp(&mut r.max_age_days, 0, 3650);
        clamp(&mut r.min_free_space_gb, 1, 10_000);
        clamp(&mut r.cqp, 1, 51);
        clamp(&mut r.bitrate_kbps, 500, 300_000);
        clamp(&mut r.max_bitrate_kbps, r.bitrate_kbps, 400_000);
        clamp(&mut r.keyframe_interval_secs, 1, 10);
        clamp(&mut r.fps, 10, 240);
        clamp(&mut r.audio_bitrate_kbps, 64, 320);
        clamp(&mut r.stall_timeout_secs, 20, 600);
        if !RECORDING_FORMATS.iter().any(|(k, _)| *k == r.format) {
            r.format = "fragmented_mp4".into();
        }
        if !["CQP", "CBR", "VBR"].contains(&r.rate_control.as_str()) {
            r.rate_control = "CQP".into();
        }
        if !r.output_resolution.is_empty() && parse_resolution(&r.output_resolution).is_none() {
            r.output_resolution.clear();
        }

        let c = &mut self.clips;
        clamp(&mut c.clip_length_secs, 5, 3600);
        clamp(&mut c.replay_max_memory_mb, 256, 65_536);
        clamp(&mut c.attribution_window_secs, 5, 3600);
        clamp(&mut c.save_cooldown_ms, 0, 10_000);
        clamp(&mut c.short_clip_secs, 5, 600);
        if c.name_template.trim().is_empty() {
            c.name_template = "{game} {date} {time}".into();
        }
        if c.unknown_game_name.trim().is_empty() {
            c.unknown_game_name = "Desktop".into();
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
        if !["top_right", "top_left", "top_center", "bottom_right", "bottom_left"]
            .contains(&ov.position.as_str())
        {
            ov.position = "top_right".into();
        }
        if !["primary", "active"].contains(&ov.monitor.as_str()) {
            ov.monitor = "primary".into();
        }

        let a = &mut self.audio;
        if !["windows_default", "none"].contains(&a.mic_fallback.as_str()) {
            a.mic_fallback = "windows_default".into();
        }
        if a.desktop_device.trim().is_empty() {
            a.desktop_device = "default".into();
        }
        if !a.mic_gain.is_finite() {
            a.mic_gain = 1.0;
        }
        clamp(&mut a.mic_gain, 0.0, 8.0);
        clamp(&mut a.mic_silence_warning_mins, 0, 600);
        dedup_case_insensitive(&mut a.mic_priority);
        dedup_case_insensitive(&mut a.voice_apps);
        dedup_case_insensitive(&mut a.music_apps);

        let gm = &mut self.games;
        clamp(&mut gm.audio_follow_delay_secs, 0, 60);
        dedup_case_insensitive(&mut gm.always_games);
        dedup_case_insensitive(&mut gm.never_games);

        let p = &mut self.player;
        clamp(&mut p.volume, 0, 100);
        clamp(&mut p.seek_small_secs, 1, 60);
        clamp(&mut p.seek_large_secs, 5, 600);
        clamp(&mut p.live_offset_secs, 2, 120);
        clamp(&mut p.timeline_hours, 1, 24 * 14);
        if !p.default_speed.is_finite() {
            p.default_speed = 1.0;
        }
        clamp(&mut p.default_speed, 0.1, 4.0);
        if !["full", "separate"].contains(&p.default_mix_mode.as_str()) {
            p.default_mix_mode = "full".into();
        }
        if p.hwdec.trim().is_empty() {
            p.hwdec = "auto-safe".into();
        }

        let e = &mut self.export;
        clamp(&mut e.quality, 1, 51);
        clamp(&mut e.audio_bitrate_kbps, 64, 512);
        clamp(&mut e.target_size_mb, 1, 4096);
        clamp(&mut e.max_height, 0, 4320);

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

    /// Settings that only take effect after OBS restarts. Used by the GUI to
    /// warn before applying, and by the service to decide whether to restart.
    pub fn obs_restart_needed(&self, other: &Config) -> bool {
        let a = &self.recording;
        let b = &other.recording;
        a.folder != b.folder
            || a.segment_minutes != b.segment_minutes
            || a.format != b.format
            || a.encoder != b.encoder
            || a.rate_control != b.rate_control
            || a.cqp != b.cqp
            || a.bitrate_kbps != b.bitrate_kbps
            || a.max_bitrate_kbps != b.max_bitrate_kbps
            || a.preset != b.preset
            || a.keyframe_interval_secs != b.keyframe_interval_secs
            || a.fps != b.fps
            || a.output_resolution != b.output_resolution
            || a.audio_bitrate_kbps != b.audio_bitrate_kbps
            || self.clips.clip_length_secs != other.clips.clip_length_secs
            || self.clips.replay_max_memory_mb != other.clips.replay_max_memory_mb
            || self.audio.multitrack != other.audio.multitrack
            || self.obs.process_priority != other.obs.process_priority
            || self.obs.websocket_port != other.obs.websocket_port
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

/// "1920x1080" -> (1920, 1080)
pub fn parse_resolution(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.trim().split_once(['x', 'X'])?;
    let w: u32 = w.trim().parse().ok()?;
    let h: u32 = h.trim().parse().ok()?;
    if (16..=16384).contains(&w) && (16..=16384).contains(&h) {
        Some((w, h))
    } else {
        None
    }
}

/// Values recovered from the old Python program's `configs.txt`.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct LegacySettings {
    pub hotkey: Option<String>,
    pub obs_path: Option<String>,
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
            "obs path" => out.obs_path = Some(v),
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
    /// old OBS split length, used to convert "keep N recordings" into hours.
    pub fn apply_legacy(&mut self, legacy: &LegacySettings, segment_minutes_legacy: u32) {
        if let Some(h) = &legacy.hotkey {
            self.hotkeys.save_clip = h.clone();
        }
        if let Some(p) = &legacy.obs_path {
            self.obs.exe_path = p.clone();
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
    fn partial_file_uses_defaults_and_ignores_unknown() {
        let text = r#"
            [recording]
            retention_hours = 24
            some_future_key = "x"
            [audio]
            voice_apps = ["Discord.exe", "discord.exe", "  TeamSpeak.exe "]
        "#;
        let mut cfg: Config = toml::from_str(text).unwrap();
        cfg.sanitize();
        assert_eq!(cfg.recording.retention_hours, 24);
        assert_eq!(cfg.recording.segment_minutes, 30);
        assert_eq!(cfg.audio.voice_apps, vec!["Discord.exe", "TeamSpeak.exe"]);
        assert_eq!(cfg.hotkeys.save_clip, "Ctrl+Shift+`");
    }

    #[test]
    fn sanitize_clamps() {
        let mut cfg = Config::default();
        cfg.recording.cqp = 0;
        cfg.recording.segment_minutes = 0;
        cfg.general.ui_scale = f32::NAN;
        cfg.overlay.position = "middle".into();
        cfg.recording.format = "avi".into();
        cfg.recording.output_resolution = "big".into();
        cfg.recording.bitrate_kbps = 10_000;
        cfg.recording.max_bitrate_kbps = 5_000;
        cfg.sanitize();
        assert_eq!(cfg.recording.cqp, 1);
        assert_eq!(cfg.recording.segment_minutes, 1);
        assert_eq!(cfg.general.ui_scale, 1.0);
        assert_eq!(cfg.overlay.position, "top_right");
        assert_eq!(cfg.recording.format, "fragmented_mp4");
        assert_eq!(cfg.recording.output_resolution, "");
        assert_eq!(cfg.recording.max_bitrate_kbps, 10_000);
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
                    obs path=C:\\Program Files\\obs-studio\\bin\\64bit\\obs64.exe\n\
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
        assert_eq!(cfg.hotkeys.save_clip, "Ctrl+Shift+`");
    }

    #[test]
    fn restart_detection() {
        let a = Config::default();
        let mut b = a.clone();
        b.player.volume = 10;
        assert!(!a.obs_restart_needed(&b));
        b.recording.cqp = 20;
        assert!(a.obs_restart_needed(&b));
    }

    #[test]
    fn resolution_parsing() {
        assert_eq!(parse_resolution("1920x1080"), Some((1920, 1080)));
        assert_eq!(parse_resolution(" 2560 X 1440 "), Some((2560, 1440)));
        assert_eq!(parse_resolution("1920"), None);
        assert_eq!(parse_resolution("0x0"), None);
    }
}

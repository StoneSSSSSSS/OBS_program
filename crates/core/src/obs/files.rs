//! Reading and writing OBS's own configuration files.
//!
//! These are only *written* while OBS is not running (OBS rewrites its config
//! on exit and would undo our changes otherwise). The service writes them just
//! before launching OBS, so OBS always starts with the right settings and
//! begins recording immediately even if the websocket is slow to come up.

use crate::config::{Config, TRACK_NAMES};
use crate::ini::IniDoc;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const FILENAME_FORMAT: &str = "%CCYY-%MM-%DD %hh-%mm-%ss";
pub const REPLAY_PREFIX: &str = "Replay";

#[derive(Debug, Clone)]
pub struct ObsConfigFiles {
    pub dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct WebsocketSettings {
    pub enabled: bool,
    pub port: u16,
    pub auth_required: bool,
    pub password: String,
}

/// What OBS is currently configured to do (used to seed our config on first run).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProfileSummary {
    pub rec_path: Option<String>,
    pub replay_secs: Option<u32>,
    pub replay_mb: Option<u32>,
    pub split_minutes: Option<u32>,
    pub split_enabled: bool,
    pub format: Option<String>,
    pub encoder: Option<String>,
    pub fps: Option<u32>,
    pub rate_control: Option<String>,
    pub cqp: Option<u32>,
    pub bitrate: Option<u32>,
    pub preset: Option<String>,
    pub audio_bitrate: Option<u32>,
}

impl ObsConfigFiles {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn from_config(cfg: &Config) -> Self {
        Self::new(crate::paths::obs_config_dir(&cfg.obs.config_dir))
    }

    /// OBS 31 keeps per-user settings in user.ini; older versions in global.ini.
    fn app_inis(&self) -> Vec<PathBuf> {
        ["user.ini", "global.ini"]
            .iter()
            .map(|f| self.dir.join(f))
            .filter(|p| p.is_file())
            .collect()
    }

    fn app_value(&self, section: &str, key: &str) -> Option<String> {
        for p in self.app_inis() {
            if let Ok(doc) = IniDoc::load(&p) {
                if let Some(v) = doc.get(section, key) {
                    if !v.trim().is_empty() {
                        return Some(v.trim().to_string());
                    }
                }
            }
        }
        None
    }

    pub fn profile_dir(&self) -> Result<PathBuf> {
        let name = self
            .app_value("Basic", "ProfileDir")
            .or_else(|| self.app_value("Basic", "Profile"))
            .unwrap_or_else(|| "Untitled".into());
        let dir = self.dir.join("basic").join("profiles").join(&name);
        if dir.is_dir() {
            return Ok(dir);
        }
        // Fall back to the only profile, if there's exactly one.
        let profiles = self.dir.join("basic").join("profiles");
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&profiles)
            .with_context(|| format!("no OBS profiles in {}", profiles.display()))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.join("basic.ini").is_file())
            .collect();
        dirs.sort();
        dirs.into_iter().next().context("no OBS profile found")
    }

    pub fn scene_collection_name(&self) -> Option<String> {
        self.app_value("Basic", "SceneCollection")
    }

    pub fn profile_ini(&self) -> Result<PathBuf> {
        Ok(self.profile_dir()?.join("basic.ini"))
    }

    pub fn websocket_json(&self) -> PathBuf {
        self.dir.join("plugin_config").join("obs-websocket").join("config.json")
    }

    pub fn read_websocket(&self) -> WebsocketSettings {
        let mut s = WebsocketSettings { port: 4455, auth_required: true, ..Default::default() };
        if let Ok(text) = std::fs::read_to_string(self.websocket_json()) {
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                s.enabled = v["server_enabled"].as_bool().unwrap_or(false);
                s.port = v["server_port"].as_u64().map(|p| p as u16).unwrap_or(4455);
                s.auth_required = v["auth_required"].as_bool().unwrap_or(true);
                s.password = v["server_password"].as_str().unwrap_or("").to_string();
            }
        }
        s
    }

    /// Make sure obs-websocket is enabled with authentication on `port`.
    /// Returns the password to use. Must be called while OBS is closed.
    pub fn ensure_websocket(&self, port: u16, preferred_password: &str) -> Result<(String, bool)> {
        let path = self.websocket_json();
        let mut v: Value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .filter(|v: &Value| v.is_object())
            .unwrap_or_else(|| json!({}));
        let mut password = v["server_password"].as_str().unwrap_or("").to_string();
        if !preferred_password.is_empty() {
            password = preferred_password.to_string();
        }
        if password.len() < 8 {
            password = crate::util::random_token(12);
        }
        let before = v.clone();
        v["server_enabled"] = json!(true);
        v["server_port"] = json!(port);
        v["auth_required"] = json!(true);
        v["server_password"] = json!(password);
        v["alerts_enabled"] = json!(v["alerts_enabled"].as_bool().unwrap_or(false));
        v["first_load"] = json!(false);
        let mut changed = before != v;
        if changed {
            crate::util::write_atomic(&path, serde_json::to_string_pretty(&v)?.as_bytes())?;
        }
        // Keep the legacy [OBSWebSocket] ini section (if any) consistent, so a
        // version of obs-websocket that migrates from it can't turn us off.
        for ini_path in self.app_inis() {
            let mut doc = IniDoc::load(&ini_path)?;
            if doc.has_section("OBSWebSocket") && doc.get("OBSWebSocket", "ServerEnabled").is_some() {
                let mut c = false;
                c |= doc.set("OBSWebSocket", "ServerEnabled", "true");
                c |= doc.set("OBSWebSocket", "ServerPort", &port.to_string());
                c |= doc.set("OBSWebSocket", "AuthRequired", "true");
                c |= doc.set("OBSWebSocket", "ServerPassword", &password);
                if c {
                    doc.save(&ini_path)?;
                    changed = true;
                }
            }
        }
        Ok((password, changed))
    }

    pub fn read_profile_summary(&self) -> Result<ProfileSummary> {
        let doc = IniDoc::load(&self.profile_ini()?)?;
        let enc_json: Value = std::fs::read_to_string(self.profile_dir()?.join("recordEncoder.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
        let fps = match doc.get_u32("Video", "FPSType") {
            Some(2) => match (doc.get_u32("Video", "FPSNum"), doc.get_u32("Video", "FPSDen")) {
                (Some(n), Some(d)) if d > 0 => Some(n / d),
                _ => None,
            },
            Some(1) => doc.get_u32("Video", "FPSInt"),
            _ => doc.get("Video", "FPSCommon").and_then(|s| s.trim().parse::<f32>().ok()).map(|f| f.round() as u32),
        };
        Ok(ProfileSummary {
            rec_path: doc.get("AdvOut", "RecFilePath").map(|s| s.replace('/', "\\")),
            replay_secs: doc.get_u32("AdvOut", "RecRBTime"),
            replay_mb: doc.get_u32("AdvOut", "RecRBSize"),
            split_minutes: doc.get_u32("AdvOut", "RecSplitFileTime"),
            split_enabled: doc.get("AdvOut", "RecSplitFile") == Some("true"),
            format: doc.get("AdvOut", "RecFormat2").map(str::to_string),
            encoder: doc.get("AdvOut", "RecEncoder").map(str::to_string),
            fps,
            rate_control: enc_json["rate_control"].as_str().map(str::to_string),
            cqp: enc_json["cqp"].as_u64().map(|v| v as u32),
            bitrate: enc_json["bitrate"].as_u64().map(|v| v as u32),
            preset: enc_json["preset2"].as_str().map(str::to_string),
            audio_bitrate: doc.get_u32("AdvOut", "Track1Bitrate"),
        })
    }

    /// Write the OBS settings ClipVault manages. Returns true if anything
    /// changed (or, with `dry_run`, would change) without writing.
    pub fn apply_settings(&self, cfg: &Config, dry_run: bool) -> Result<bool> {
        let ini_path = self.profile_ini()?;
        let mut doc = IniDoc::load(&ini_path)?;
        let mut changed = false;
        for (section, key, value) in desired_profile_values(cfg) {
            changed |= doc.set(section, &key, &value);
        }
        // Remove OBS's own replay-save hotkey: ClipVault owns that hotkey, and
        // both firing would save every clip twice.
        let rb = doc.get("Hotkeys", "ReplayBuffer").map(str::to_string);
        if let Some(rb) = rb {
            let cleared = clear_replay_save_binding(&rb);
            if cleared != rb {
                doc.set("Hotkeys", "ReplayBuffer", &cleared);
                changed = true;
            }
        }
        if changed && !dry_run {
            backup_once(&ini_path);
            doc.save(&ini_path)?;
        }

        let enc_path = self.profile_dir()?.join("recordEncoder.json");
        let existing: Value = std::fs::read_to_string(&enc_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_else(|| json!({}));
        let desired = desired_encoder_json(&existing, cfg);
        if desired != existing {
            if dry_run {
                return Ok(true);
            }
            backup_once(&enc_path);
            crate::util::write_atomic(&enc_path, serde_json::to_string_pretty(&desired)?.as_bytes())?;
            changed = true;
        }

        // Process priority lives in the app config.
        let prio = match cfg.obs.process_priority.as_str() {
            "high" => "High",
            "normal" => "Normal",
            _ => "AboveNormal",
        };
        for p in self.app_inis() {
            let mut d = IniDoc::load(&p)?;
            if d.get("General", "ProcessPriority").is_some() && d.set("General", "ProcessPriority", prio) {
                if !dry_run {
                    d.save(&p)?;
                }
                changed = true;
            }
        }
        Ok(changed)
    }
}

/// Keep a one-time backup of an OBS file before we first modify it.
fn backup_once(path: &Path) {
    let backup = path.with_extension(format!(
        "{}.clipvault-backup",
        path.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    if !backup.exists() && path.exists() {
        let _ = std::fs::copy(path, &backup);
    }
}

/// `{"ReplayBuffer.Save":[...], ...}` -> same JSON with the Save bindings emptied.
pub fn clear_replay_save_binding(json_text: &str) -> String {
    match serde_json::from_str::<Value>(json_text) {
        Ok(mut v) if v.is_object() => {
            if v.get("ReplayBuffer.Save").map(|b| b.as_array().map(|a| !a.is_empty()).unwrap_or(true)) == Some(true) {
                v["ReplayBuffer.Save"] = json!([]);
                v.to_string()
            } else {
                json_text.to_string()
            }
        }
        _ => json_text.to_string(),
    }
}

/// The profile (basic.ini) key/values ClipVault enforces.
pub fn desired_profile_values(cfg: &Config) -> Vec<(&'static str, String, String)> {
    let r = &cfg.recording;
    let mut v: Vec<(&'static str, String, String)> = Vec::new();
    let mut put = |s: &'static str, k: &str, val: String| v.push((s, k.to_string(), val));
    put("Output", "Mode", "Advanced".into());
    put("Output", "FilenameFormatting", FILENAME_FORMAT.into());
    put("AdvOut", "RecType", "Standard".into());
    put("AdvOut", "RecFilePath", r.folder.replace('\\', "/"));
    put("AdvOut", "RecFormat2", r.format.clone());
    put("AdvOut", "RecEncoder", r.encoder.clone());
    let tracks = if cfg.audio.multitrack { 0b11_1111 } else { 0b1 };
    put("AdvOut", "RecTracks", tracks.to_string());
    put("AdvOut", "RecSplitFile", "true".into());
    put("AdvOut", "RecSplitFileType", "Time".into());
    put("AdvOut", "RecSplitFileTime", r.segment_minutes.to_string());
    put("AdvOut", "RecRB", "true".into());
    put("AdvOut", "RecRBTime", cfg.clips.clip_length_secs.to_string());
    put("AdvOut", "RecRBSize", cfg.clips.replay_max_memory_mb.to_string());
    put("AdvOut", "RecRBPrefix", REPLAY_PREFIX.into());
    put("AdvOut", "RecRBSuffix", String::new());
    for (i, name) in TRACK_NAMES.iter().enumerate() {
        put("AdvOut", &format!("Track{}Bitrate", i + 1), r.audio_bitrate_kbps.to_string());
        put("AdvOut", &format!("Track{}Name", i + 1), name.to_string());
    }
    match crate::config::parse_resolution(&r.output_resolution) {
        Some((w, h)) => {
            put("AdvOut", "RecRescale", "true".into());
            put("AdvOut", "RecRescaleRes", format!("{w}x{h}"));
        }
        None => put("AdvOut", "RecRescale", "false".into()),
    }
    put("Video", "FPSType", "2".into());
    put("Video", "FPSNum", r.fps.to_string());
    put("Video", "FPSDen", "1".into());
    v
}

/// Merge our encoder settings into OBS's recordEncoder.json, keeping any other keys.
pub fn desired_encoder_json(existing: &Value, cfg: &Config) -> Value {
    let r = &cfg.recording;
    let mut v = if existing.is_object() { existing.clone() } else { json!({}) };
    let x264 = r.encoder.contains("x264");
    if x264 {
        let rc = if r.rate_control == "CQP" { "CRF" } else { r.rate_control.as_str() };
        v["rate_control"] = json!(rc);
        v["crf"] = json!(r.cqp);
        v["bitrate"] = json!(r.bitrate_kbps);
        v["preset"] = json!(if r.preset.starts_with('p') { "veryfast" } else { r.preset.as_str() });
    } else {
        v["rate_control"] = json!(r.rate_control);
        v["cqp"] = json!(r.cqp);
        v["bitrate"] = json!(r.bitrate_kbps);
        v["max_bitrate"] = json!(r.max_bitrate_kbps);
        v["preset2"] = json!(r.preset);
    }
    v["keyint_sec"] = json!(r.keyframe_interval_secs);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_obs_dir() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        std::fs::write(
            d.join("user.ini"),
            "\u{FEFF}[General]\nConfirmOnExit=true\n\n[Basic]\nProfile=Untitled\nProfileDir=Untitled\nSceneCollection=Untitled\n\n[OBSWebSocket]\nFirstLoad=false\nServerEnabled=false\nServerPort=4455\nAuthRequired=true\nServerPassword=oldpassword123\n",
        )
        .unwrap();
        std::fs::write(d.join("global.ini"), "[General]\nProcessPriority=Normal\n\n[OBSWebSocket]\n").unwrap();
        let prof = d.join("basic").join("profiles").join("Untitled");
        std::fs::create_dir_all(&prof).unwrap();
        std::fs::write(
            prof.join("basic.ini"),
            "[General]\nName=Untitled\n\n[Video]\nFPSType=2\nFPSNum=60\nFPSDen=1\n\n[Output]\nMode=Advanced\n\n[AdvOut]\nRecFilePath=I:/OBS/Recordings\nRecFormat2=fragmented_mp4\nRecEncoder=jim_nvenc\nRecTracks=1\nRecSplitFile=true\nRecSplitFileTime=60\nRecRB=true\nRecRBTime=420\nRecRBSize=32768\nTrack1Bitrate=160\n\n[Hotkeys]\nReplayBuffer={\"ReplayBuffer.Save\":[{\"shift\":true,\"control\":true,\"key\":\"OBS_KEY_ASCIITILDE\"}]}\nOBSBasic.StartRecording={\"bindings\":[]}\n",
        )
        .unwrap();
        std::fs::write(prof.join("recordEncoder.json"), r#"{"rate_control":"CQP","cqp":25,"preset2":"p2","psycho_aq":false,"gpu":0}"#).unwrap();
        let ws = d.join("plugin_config").join("obs-websocket");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("config.json"), r#"{"alerts_enabled":false,"auth_required":true,"first_load":false,"server_enabled":false,"server_password":"secretpw99","server_port":4455}"#).unwrap();
        tmp
    }

    #[test]
    fn reads_profile_summary() {
        let tmp = fake_obs_dir();
        let f = ObsConfigFiles::new(tmp.path());
        let s = f.read_profile_summary().unwrap();
        assert_eq!(s.rec_path.as_deref(), Some(r"I:\OBS\Recordings"));
        assert_eq!(s.replay_secs, Some(420));
        assert_eq!(s.split_minutes, Some(60));
        assert!(s.split_enabled);
        assert_eq!(s.fps, Some(60));
        assert_eq!(s.cqp, Some(25));
        assert_eq!(s.preset.as_deref(), Some("p2"));
        assert_eq!(s.audio_bitrate, Some(160));
    }

    #[test]
    fn enables_websocket_keeping_password_and_syncing_ini() {
        let tmp = fake_obs_dir();
        let f = ObsConfigFiles::new(tmp.path());
        let (pw, changed) = f.ensure_websocket(4455, "").unwrap();
        assert!(changed);
        assert_eq!(pw, "secretpw99");
        let ws = f.read_websocket();
        assert!(ws.enabled && ws.auth_required);
        assert_eq!(ws.password, "secretpw99");
        let user = IniDoc::load(&tmp.path().join("user.ini")).unwrap();
        assert_eq!(user.get("OBSWebSocket", "ServerEnabled"), Some("true"));
        assert_eq!(user.get("OBSWebSocket", "ServerPassword"), Some("secretpw99"));
        // global.ini's empty section is left alone.
        let global = std::fs::read_to_string(tmp.path().join("global.ini")).unwrap();
        assert!(!global.contains("ServerEnabled"));
        // Second call is a no-op.
        let (_, changed2) = f.ensure_websocket(4455, "").unwrap();
        assert!(!changed2);
    }

    #[test]
    fn generates_password_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let f = ObsConfigFiles::new(tmp.path());
        let (pw, changed) = f.ensure_websocket(4460, "").unwrap();
        assert!(changed);
        assert_eq!(pw.len(), 24);
        assert_eq!(f.read_websocket().port, 4460);
    }

    #[test]
    fn applies_managed_settings_preserving_others() {
        let tmp = fake_obs_dir();
        let f = ObsConfigFiles::new(tmp.path());
        let mut cfg = Config::default();
        cfg.recording.segment_minutes = 30;
        cfg.clips.clip_length_secs = 300;
        cfg.recording.cqp = 22;
        assert!(f.apply_settings(&cfg, true).unwrap(), "dry run reports pending changes");
        let untouched = IniDoc::load(&f.profile_ini().unwrap()).unwrap();
        assert_eq!(untouched.get("AdvOut", "RecTracks"), Some("1"), "dry run writes nothing");
        assert!(f.apply_settings(&cfg, false).unwrap());
        let doc = IniDoc::load(&f.profile_ini().unwrap()).unwrap();
        assert_eq!(doc.get("AdvOut", "RecTracks"), Some("63"));
        assert_eq!(doc.get("AdvOut", "RecSplitFileTime"), Some("30"));
        assert_eq!(doc.get("AdvOut", "RecRBTime"), Some("300"));
        assert_eq!(doc.get("AdvOut", "Track5Name"), Some("Microphone"));
        assert_eq!(doc.get("Output", "FilenameFormatting"), Some(FILENAME_FORMAT));
        assert_eq!(doc.get("General", "Name"), Some("Untitled"));
        assert_eq!(doc.get("Hotkeys", "OBSBasic.StartRecording"), Some("{\"bindings\":[]}"));
        let rb: Value = serde_json::from_str(doc.get("Hotkeys", "ReplayBuffer").unwrap()).unwrap();
        assert_eq!(rb["ReplayBuffer.Save"], json!([]));
        let enc: Value = serde_json::from_str(
            &std::fs::read_to_string(f.profile_dir().unwrap().join("recordEncoder.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(enc["cqp"], 22);
        assert_eq!(enc["psycho_aq"], false);
        assert_eq!(enc["keyint_sec"], 2);
        let global = IniDoc::load(&tmp.path().join("global.ini")).unwrap();
        assert_eq!(global.get("General", "ProcessPriority"), Some("AboveNormal"));
        // Backups were made, and re-applying is a no-op.
        assert!(f.profile_dir().unwrap().join("basic.ini.clipvault-backup").exists());
        assert!(!f.apply_settings(&cfg, false).unwrap());
        assert!(!f.apply_settings(&cfg, true).unwrap());
    }

    #[test]
    fn x264_encoder_json() {
        let mut cfg = Config::default();
        cfg.recording.encoder = "obs_x264".into();
        let v = desired_encoder_json(&json!({"other": 1}), &cfg);
        assert_eq!(v["rate_control"], "CRF");
        assert_eq!(v["crf"], 25);
        assert_eq!(v["preset"], "veryfast");
        assert_eq!(v["other"], 1);
    }

    #[test]
    fn replay_binding_cleared() {
        let s = r#"{"ReplayBuffer.Save":[{"key":"OBS_KEY_F1"}],"ReplayBuffer.Start":[]}"#;
        let out: Value = serde_json::from_str(&clear_replay_save_binding(s)).unwrap();
        assert_eq!(out["ReplayBuffer.Save"], json!([]));
        assert_eq!(out["ReplayBuffer.Start"], json!([]));
        let already = r#"{"ReplayBuffer.Save":[]}"#;
        assert_eq!(clear_replay_save_binding(already), already);
        assert_eq!(clear_replay_save_binding("garbage"), "garbage");
    }
}

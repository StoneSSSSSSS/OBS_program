//! Configuring OBS's audio inputs over the websocket.
//!
//! Track layout (see `cv_core::config::TRACK_NAMES`):
//!   1 Full mix   = desktop audio + mic (+ apps that play on another device)
//!   2 Game       = app capture following the focused game
//!   3 Voice chat = app capture of Discord etc.
//!   4 Music      = app capture of Spotify etc.
//!   5 Microphone
//!   6 Desktop    = everything the PC plays on the desktop-audio device

use cv_core::config::{Config, TRACK_DESKTOP, TRACK_FULL, TRACK_GAME, TRACK_MIC, TRACK_MUSIC, TRACK_VOICE};
use cv_core::obs::protocol::{parse_window_match_string, track_bit, tracks_mask, tracks_object, window_match_string, window_priority};
use cv_core::obs::{client::ObsError, ObsClient};
use serde_json::{json, Value};
use std::collections::HashMap;

pub const GAME_INPUT: &str = "Game Audio";
pub const MIC_INPUT_DEFAULT: &str = "Microphone";
pub const VOICE_PREFIX: &str = "Voice · ";
pub const MUSIC_PREFIX: &str = "Music · ";
pub const NOISE_FILTER: &str = "ClipVault Noise Suppression";
const APP_KIND: &str = "wasapi_process_output_capture";

/// Names of the inputs ClipVault manages in the running OBS.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Managed {
    pub scene: String,
    pub desktop: Option<String>,
    pub mic: Option<String>,
    /// (input name, exe, base track)
    pub apps: Vec<(String, String, usize)>,
}

#[derive(Debug, Clone)]
struct InputEntry {
    name: String,
    kind: String,
}

fn list_inputs(obs: &ObsClient) -> Result<Vec<InputEntry>, ObsError> {
    let v = obs.request("GetInputList", None)?;
    Ok(v["inputs"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|i| InputEntry {
                    name: i["inputName"].as_str().unwrap_or("").to_string(),
                    kind: i["unversionedInputKind"].as_str().or(i["inputKind"].as_str()).unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default())
}

pub fn current_scene(obs: &ObsClient) -> Result<String, ObsError> {
    let v = obs.request("GetCurrentProgramScene", None)?;
    Ok(v["sceneName"].as_str().or(v["currentProgramSceneName"].as_str()).unwrap_or("").to_string())
}

/// Make sure `input` is shown in `scene` (hidden scene items are silent).
fn ensure_in_scene(obs: &ObsClient, scene: &str, input: &str) -> Result<(), ObsError> {
    match obs.request("GetSceneItemId", Some(json!({"sceneName": scene, "sourceName": input}))) {
        Ok(v) => {
            let id = v["sceneItemId"].as_i64().unwrap_or(0);
            let en = obs.request("GetSceneItemEnabled", Some(json!({"sceneName": scene, "sceneItemId": id})))?;
            if en["sceneItemEnabled"].as_bool() == Some(false) {
                obs.request("SetSceneItemEnabled", Some(json!({"sceneName": scene, "sceneItemId": id, "sceneItemEnabled": true})))?;
            }
            Ok(())
        }
        Err(ObsError::Request { .. }) => {
            obs.request("CreateSceneItem", Some(json!({"sceneName": scene, "sourceName": input, "sceneItemEnabled": true})))?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn create_input(obs: &ObsClient, scene: &str, name: &str, kind: &str, settings: Value) -> Result<(), ObsError> {
    obs.request(
        "CreateInput",
        Some(json!({"sceneName": scene, "inputName": name, "inputKind": kind, "inputSettings": settings, "sceneItemEnabled": true})),
    )?;
    tracing::info!("created OBS input \"{name}\" ({kind})");
    Ok(())
}

pub fn get_tracks(obs: &ObsClient, input: &str) -> Result<u32, ObsError> {
    let v = obs.request("GetInputAudioTracks", Some(json!({"inputName": input})))?;
    Ok(tracks_mask(&v["inputAudioTracks"]))
}

pub fn set_tracks(obs: &ObsClient, input: &str, mask: u32) -> Result<bool, ObsError> {
    if get_tracks(obs, input)? == mask {
        return Ok(false);
    }
    obs.request("SetInputAudioTracks", Some(json!({"inputName": input, "inputAudioTracks": tracks_object(mask)})))?;
    Ok(true)
}

pub fn get_settings(obs: &ObsClient, input: &str) -> Result<Value, ObsError> {
    let v = obs.request("GetInputSettings", Some(json!({"inputName": input})))?;
    Ok(v["inputSettings"].clone())
}

pub fn set_settings(obs: &ObsClient, input: &str, settings: Value) -> Result<(), ObsError> {
    obs.request("SetInputSettings", Some(json!({"inputName": input, "inputSettings": settings, "overlay": true})))?;
    Ok(())
}

fn app_label(exe: &str) -> String {
    let stem = exe.trim().trim_end_matches(".exe").trim_end_matches(".EXE");
    let mut c = stem.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => exe.to_string(),
    }
}

/// Create/repair all managed inputs. `routing` says which app exes should
/// also feed the full mix (because they play on a different device).
pub fn ensure_inputs(obs: &ObsClient, cfg: &Config, mic_meta: Option<&str>, routing: &HashMap<String, bool>) -> Result<Managed, ObsError> {
    let scene = current_scene(obs)?;
    let inputs = list_inputs(obs)?;
    let has = |n: &str| inputs.iter().any(|i| i.name == n);
    let specials = obs.request("GetSpecialInputs", None)?;
    let multitrack = cfg.audio.multitrack;
    let mut m = Managed { scene: scene.clone(), ..Default::default() };

    // ---- desktop audio
    let desktop = specials["desktop1"]
        .as_str()
        .filter(|s| !s.is_empty() && has(s))
        .map(str::to_string)
        .or_else(|| inputs.iter().find(|i| i.kind == "wasapi_output_capture").map(|i| i.name.clone()));
    let desktop = match desktop {
        Some(d) => d,
        None => {
            create_input(obs, &scene, "Desktop Audio", "wasapi_output_capture", json!({"device_id": "default"}))?;
            "Desktop Audio".to_string()
        }
    };
    let dmask = if multitrack { track_bit(TRACK_FULL) | track_bit(TRACK_DESKTOP) } else { track_bit(TRACK_FULL) };
    set_tracks(obs, &desktop, dmask)?;
    m.desktop = Some(desktop);

    // ---- microphone
    let mic = mic_meta
        .filter(|n| has(n))
        .map(str::to_string)
        .or_else(|| specials["mic1"].as_str().filter(|s| !s.is_empty() && has(s)).map(str::to_string))
        .or_else(|| inputs.iter().find(|i| i.kind == "wasapi_input_capture").map(|i| i.name.clone()));
    let mut mic = match mic {
        Some(n) => n,
        None => {
            create_input(obs, &scene, MIC_INPUT_DEFAULT, "wasapi_input_capture", json!({"device_id": "default"}))?;
            MIC_INPUT_DEFAULT.to_string()
        }
    };
    if mic == "Audio Input Capture" && !has(MIC_INPUT_DEFAULT) {
        if obs.request("SetInputName", Some(json!({"inputName": mic, "newInputName": MIC_INPUT_DEFAULT}))).is_ok() {
            mic = MIC_INPUT_DEFAULT.to_string();
        }
    }
    let mmask = if multitrack { track_bit(TRACK_FULL) | track_bit(TRACK_MIC) } else { track_bit(TRACK_FULL) };
    set_tracks(obs, &mic, mmask)?;
    // Mic gain (only touch it if the user changed the setting from 1.0).
    if (cfg.audio.mic_gain - 1.0).abs() > 0.001 {
        let _ = obs.request("SetInputVolume", Some(json!({"inputName": mic, "inputVolumeMul": cfg.audio.mic_gain})));
    }
    ensure_noise_filter(obs, &mic, cfg.audio.mic_noise_suppression);
    m.mic = Some(mic);

    // ---- app captures
    let mut wanted: Vec<(String, String, usize)> = Vec::new();
    if multitrack {
        if cfg.audio.game_audio_capture {
            wanted.push((GAME_INPUT.to_string(), String::new(), TRACK_GAME));
        }
        for app in &cfg.audio.voice_apps {
            wanted.push((format!("{VOICE_PREFIX}{}", app_label(app)), app.clone(), TRACK_VOICE));
        }
        for app in &cfg.audio.music_apps {
            wanted.push((format!("{MUSIC_PREFIX}{}", app_label(app)), app.clone(), TRACK_MUSIC));
        }
    }
    // Remove managed inputs that are no longer configured.
    for i in &inputs {
        let managed = i.name == GAME_INPUT || i.name.starts_with(VOICE_PREFIX) || i.name.starts_with(MUSIC_PREFIX);
        if managed && i.kind == APP_KIND && !wanted.iter().any(|(n, _, _)| *n == i.name) {
            tracing::info!("removing OBS input \"{}\"", i.name);
            let _ = obs.request("RemoveInput", Some(json!({"inputName": i.name})));
        }
    }
    for (name, exe, track) in &wanted {
        if !has(name) {
            let window = if exe.is_empty() { String::new() } else { window_match_string("", "", exe) };
            create_input(obs, &scene, name, APP_KIND, json!({"window": window, "priority": window_priority::EXE}))?;
        } else if !exe.is_empty() {
            // Keep the configured exe (user may have edited the list).
            let s = get_settings(obs, name)?;
            let current_exe = s["window"].as_str().and_then(parse_window_match_string).map(|(_, _, e)| e).unwrap_or_default();
            if !current_exe.eq_ignore_ascii_case(exe) {
                set_settings(obs, name, json!({"window": window_match_string("", "", exe), "priority": window_priority::EXE}))?;
            }
        }
        ensure_in_scene(obs, &scene, name)?;
        let exe_key = if exe.is_empty() { None } else { Some(exe.to_lowercase()) };
        let mut mask = track_bit(*track);
        if let Some(k) = &exe_key {
            if routing.get(k).copied().unwrap_or(false) {
                mask |= track_bit(TRACK_FULL);
            }
        }
        set_tracks(obs, name, mask)?;
        m.apps.push((name.clone(), exe.clone(), *track));
    }
    Ok(m)
}

fn ensure_noise_filter(obs: &ObsClient, mic: &str, enabled: bool) {
    let exists = obs
        .request("GetSourceFilter", Some(json!({"sourceName": mic, "filterName": NOISE_FILTER})))
        .is_ok();
    if enabled && !exists {
        let r = obs.request(
            "CreateSourceFilter",
            Some(json!({"sourceName": mic, "filterName": NOISE_FILTER, "filterKind": "noise_suppress_filter_v2", "filterSettings": {"method": "rnnoise"}})),
        );
        if let Err(e) = r {
            tracing::warn!("could not add noise suppression: {e}");
        }
    } else if !enabled && exists {
        let _ = obs.request("RemoveSourceFilter", Some(json!({"sourceName": mic, "filterName": NOISE_FILTER})));
    }
}

/// Point the game-audio capture at a game. Returns true if changed.
pub fn set_game_target(obs: &ObsClient, exe: &str, title: &str, class: &str) -> Result<bool, ObsError> {
    let want = window_match_string(title, class, exe);
    let cur = get_settings(obs, GAME_INPUT)?;
    let cur_exe = cur["window"].as_str().and_then(parse_window_match_string).map(|(_, _, e)| e).unwrap_or_default();
    if cur_exe.eq_ignore_ascii_case(exe) {
        return Ok(false);
    }
    set_settings(obs, GAME_INPUT, json!({"window": want, "priority": window_priority::EXE}))?;
    Ok(true)
}

/// Point a wasapi input at a device id. Returns true if it changed.
pub fn set_device(obs: &ObsClient, input: &str, device_id: &str) -> Result<bool, ObsError> {
    let cur = get_settings(obs, input)?;
    if cur["device_id"].as_str() == Some(device_id) {
        return Ok(false);
    }
    set_settings(obs, input, json!({"device_id": device_id}))?;
    Ok(true)
}

/// Update which app captures feed the full mix.
pub fn apply_routing(obs: &ObsClient, m: &Managed, routing: &HashMap<String, bool>, game_exe: Option<&str>) -> Result<(), ObsError> {
    for (name, exe, track) in &m.apps {
        let exe_key = if exe.is_empty() { game_exe.map(|s| s.to_lowercase()) } else { Some(exe.to_lowercase()) };
        let mut mask = track_bit(*track);
        if let Some(k) = exe_key {
            if routing.get(&k).copied().unwrap_or(false) {
                mask |= track_bit(TRACK_FULL);
            }
        }
        if set_tracks(obs, name, mask)? {
            tracing::info!("\"{name}\" full-mix routing -> {}", mask & 1 == 1);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(app_label("Discord.exe"), "Discord");
        assert_eq!(app_label("spotify.exe"), "Spotify");
        assert_eq!(app_label(""), "");
    }
}

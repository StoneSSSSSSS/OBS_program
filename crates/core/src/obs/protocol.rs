//! obs-websocket 5.x protocol details.

use base64::Engine;
use sha2::{Digest, Sha256};

pub const OP_HELLO: u64 = 0;
pub const OP_IDENTIFY: u64 = 1;
pub const OP_IDENTIFIED: u64 = 2;
pub const OP_REIDENTIFY: u64 = 3;
pub const OP_EVENT: u64 = 5;
pub const OP_REQUEST: u64 = 6;
pub const OP_REQUEST_RESPONSE: u64 = 7;

pub mod subs {
    pub const GENERAL: u32 = 1 << 0;
    pub const CONFIG: u32 = 1 << 1;
    pub const SCENES: u32 = 1 << 2;
    pub const INPUTS: u32 = 1 << 3;
    pub const TRANSITIONS: u32 = 1 << 4;
    pub const FILTERS: u32 = 1 << 5;
    pub const OUTPUTS: u32 = 1 << 6;
    pub const SCENE_ITEMS: u32 = 1 << 7;
    pub const MEDIA_INPUTS: u32 = 1 << 8;
    pub const VENDORS: u32 = 1 << 9;
    pub const UI: u32 = 1 << 10;
    pub const ALL: u32 = (1 << 11) - 1;
    pub const INPUT_VOLUME_METERS: u32 = 1 << 16;
    pub const INPUT_ACTIVE_STATE_CHANGED: u32 = 1 << 17;
}

/// obs-websocket authentication string:
/// `base64(sha256(base64(sha256(password + salt)) + challenge))`.
pub fn auth_string(password: &str, salt: &str, challenge: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD;
    let secret = b64.encode(Sha256::digest(format!("{password}{salt}").as_bytes()));
    b64.encode(Sha256::digest(format!("{secret}{challenge}").as_bytes()))
}

/// OBS's window-match string `title:class:exe`, with `#` and `:` escaped the
/// way OBS's `window-helpers.c` does (`#` -> `#22`, `:` -> `#3A`).
pub fn window_match_string(title: &str, class: &str, exe: &str) -> String {
    fn enc(s: &str) -> String {
        s.replace('#', "#22").replace(':', "#3A")
    }
    format!("{}:{}:{}", enc(title), enc(class), enc(exe))
}

/// Decode an OBS window-match string back into (title, class, exe).
pub fn parse_window_match_string(s: &str) -> Option<(String, String, String)> {
    fn dec(s: &str) -> String {
        s.replace("#3A", ":").replace("#22", "#")
    }
    let mut it = s.split(':');
    let t = it.next()?;
    let c = it.next()?;
    let e = it.next()?;
    if it.next().is_some() {
        return None;
    }
    Some((dec(t), dec(c), dec(e)))
}

/// `window_priority` values for OBS window/app-audio capture.
pub mod window_priority {
    pub const CLASS: i64 = 0;
    pub const TITLE: i64 = 1;
    pub const EXE: i64 = 2;
}

/// Bitmask -> obs-websocket `inputAudioTracks` object (`{"1": true, ...}`).
pub fn tracks_object(mask: u32) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for t in 1..=6u32 {
        m.insert(t.to_string(), serde_json::Value::Bool(mask & (1 << (t - 1)) != 0));
    }
    serde_json::Value::Object(m)
}

/// obs-websocket `inputAudioTracks` object -> bitmask.
pub fn tracks_mask(v: &serde_json::Value) -> u32 {
    let mut mask = 0;
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            if let (Ok(t), Some(true)) = (k.parse::<u32>(), val.as_bool()) {
                if (1..=6).contains(&t) {
                    mask |= 1 << (t - 1);
                }
            }
        }
    }
    mask
}

pub fn track_bit(track: usize) -> u32 {
    1 << (track - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_matches_reference_implementation() {
        // Independent reference computed with Python's hashlib/base64 following
        // the obs-websocket protocol documentation.
        let auth = auth_string(
            "supersecretpassword",
            "lZWB/DZB0HTDmn4DOWO7PLuw2bdz6vPzx3GmL3qLoA4=",
            "+IxH4CnCiqpX1rM9scsNynZzbOe4KhDeYcTNS3PDaeY=",
        );
        assert_eq!(auth, "d17px8h5vYkKyX8CkQBygTJBCK1SOVZCt8g5SC8E7LM=");
    }

    #[test]
    fn window_strings() {
        let s = window_match_string("A:B#C", "Cls", "game.exe");
        assert_eq!(s, "A#3AB#22C:Cls:game.exe");
        assert_eq!(parse_window_match_string(&s), Some(("A:B#C".into(), "Cls".into(), "game.exe".into())));
        assert_eq!(parse_window_match_string("a:b"), None);
    }

    #[test]
    fn track_masks() {
        let v = tracks_object(0b100001);
        assert_eq!(v["1"], true);
        assert_eq!(v["2"], false);
        assert_eq!(v["6"], true);
        assert_eq!(tracks_mask(&v), 0b100001);
        assert_eq!(tracks_mask(&serde_json::json!({"1": true, "3": true, "9": true, "x": true})), 0b101);
        assert_eq!(track_bit(5), 16);
    }
}

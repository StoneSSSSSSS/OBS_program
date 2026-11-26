//! Hotkey strings like `Ctrl+Shift+\`` or `Alt+Mouse5`.
//!
//! Keyboard keys map to Windows virtual-key codes (used with RegisterHotKey);
//! mouse side buttons are handled by the service through raw input.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Middle,
    X1,
    X2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Trigger {
    /// Windows virtual-key code.
    Key(u32),
    Mouse(MouseButton),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hotkey {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
    pub trigger: Trigger,
}

/// (canonical name, virtual key, aliases)
const KEYS: &[(&str, u32, &[&str])] = &[
    ("`", 0xC0, &["grave", "backtick", "tilde", "~", "oem3"]),
    ("-", 0xBD, &["minus"]),
    ("=", 0xBB, &["equals", "plus"]),
    ("[", 0xDB, &["lbracket"]),
    ("]", 0xDD, &["rbracket"]),
    ("\\", 0xDC, &["backslash"]),
    (";", 0xBA, &["semicolon"]),
    ("'", 0xDE, &["quote", "apostrophe"]),
    (",", 0xBC, &["comma"]),
    (".", 0xBE, &["period", "dot"]),
    ("/", 0xBF, &["slash"]),
    ("Space", 0x20, &["spacebar"]),
    ("Enter", 0x0D, &["return"]),
    ("Tab", 0x09, &[]),
    ("Esc", 0x1B, &["escape"]),
    ("Backspace", 0x08, &[]),
    ("Insert", 0x2D, &["ins"]),
    ("Delete", 0x2E, &["del"]),
    ("Home", 0x24, &[]),
    ("End", 0x23, &[]),
    ("PageUp", 0x21, &["pgup"]),
    ("PageDown", 0x22, &["pgdn"]),
    ("Left", 0x25, &[]),
    ("Up", 0x26, &[]),
    ("Right", 0x27, &[]),
    ("Down", 0x28, &[]),
    ("PrintScreen", 0x2C, &["prtsc", "printscr"]),
    ("Pause", 0x13, &["break"]),
    ("ScrollLock", 0x91, &[]),
    ("NumMultiply", 0x6A, &["num*", "multiply"]),
    ("NumAdd", 0x6B, &["num+", "add"]),
    ("NumSubtract", 0x6D, &["num-", "subtract"]),
    ("NumDecimal", 0x6E, &["num.", "decimal"]),
    ("NumDivide", 0x6F, &["num/", "divide"]),
];

fn key_from_name(name: &str) -> Option<u32> {
    let lower = name.to_lowercase();
    // Single letter or digit.
    if name.chars().count() == 1 {
        let c = name.chars().next().unwrap().to_ascii_uppercase();
        if c.is_ascii_uppercase() || c.is_ascii_digit() {
            return Some(c as u32);
        }
    }
    // F1..F24
    if let Some(n) = lower.strip_prefix('f').and_then(|r| r.parse::<u32>().ok()) {
        if (1..=24).contains(&n) {
            return Some(0x70 + n - 1);
        }
    }
    // Num0..Num9
    if let Some(n) = lower
        .strip_prefix("numpad")
        .or_else(|| lower.strip_prefix("num"))
        .and_then(|r| r.parse::<u32>().ok())
    {
        if n <= 9 {
            return Some(0x60 + n);
        }
    }
    for (canon, vk, aliases) in KEYS {
        if canon.to_lowercase() == lower || aliases.iter().any(|a| *a == lower) {
            return Some(*vk);
        }
    }
    None
}

pub fn key_name(vk: u32) -> String {
    match vk {
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(vk).unwrap().to_string(),
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        0x60..=0x69 => format!("Num{}", vk - 0x60),
        _ => KEYS
            .iter()
            .find(|(_, v, _)| *v == vk)
            .map(|(n, _, _)| n.to_string())
            .unwrap_or_else(|| format!("VK{vk:02X}")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HotkeyError {
    #[error("empty hotkey")]
    Empty,
    #[error("unknown key \"{0}\"")]
    UnknownKey(String),
    #[error("hotkey needs exactly one non-modifier key")]
    NoKey,
    #[error("a hotkey without Ctrl/Alt/Shift/Win would block normal typing")]
    NeedsModifier,
}

impl Hotkey {
    /// Parse e.g. `Ctrl+Shift+\``, `ctrl + alt + f9`, `Mouse5`, `Alt+Mouse4`.
    /// Returns `Ok(None)` for an empty string (hotkey disabled).
    pub fn parse(s: &str) -> Result<Option<Hotkey>, HotkeyError> {
        let s = s.trim();
        if s.is_empty() {
            return Ok(None);
        }
        // Split on '+' but allow the '+' key itself as "Plus"/"=".
        let parts: Vec<&str> = s.split('+').map(str::trim).filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            return Err(HotkeyError::Empty);
        }
        let (mut ctrl, mut shift, mut alt, mut win) = (false, false, false, false);
        let mut trigger = None;
        for p in parts {
            match p.to_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "shift" => shift = true,
                "alt" => alt = true,
                "win" | "super" | "meta" | "cmd" => win = true,
                "mouse4" | "xbutton1" | "mousex1" | "mouseback" => trigger = Some(Trigger::Mouse(MouseButton::X1)),
                "mouse5" | "xbutton2" | "mousex2" | "mouseforward" => trigger = Some(Trigger::Mouse(MouseButton::X2)),
                "mouse3" | "mousemiddle" | "middlemouse" => trigger = Some(Trigger::Mouse(MouseButton::Middle)),
                _ => {
                    if trigger.is_some() {
                        return Err(HotkeyError::NoKey);
                    }
                    trigger = Some(Trigger::Key(key_from_name(p).ok_or_else(|| HotkeyError::UnknownKey(p.to_string()))?));
                }
            }
        }
        let trigger = trigger.ok_or(HotkeyError::NoKey)?;
        let hk = Hotkey { ctrl, shift, alt, win, trigger };
        // Bare keys are allowed only for keys nobody types with (F13-F24, Pause...)
        // and for mouse side buttons.
        if !(ctrl || shift || alt || win) {
            let ok_bare = match trigger {
                Trigger::Mouse(MouseButton::Middle) => false,
                Trigger::Mouse(_) => true,
                Trigger::Key(vk) => (0x7C..=0x87).contains(&vk) || vk == 0x13 || vk == 0x91 || vk == 0x2C,
            };
            if !ok_bare {
                return Err(HotkeyError::NeedsModifier);
            }
        }
        Ok(Some(hk))
    }

    /// RegisterHotKey modifier flags (MOD_ALT=1, MOD_CONTROL=2, MOD_SHIFT=4, MOD_WIN=8).
    pub fn win32_modifiers(&self) -> u32 {
        (self.alt as u32) | ((self.ctrl as u32) << 1) | ((self.shift as u32) << 2) | ((self.win as u32) << 3)
    }
}

impl fmt::Display for Hotkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts: Vec<String> = Vec::new();
        if self.ctrl {
            parts.push("Ctrl".into());
        }
        if self.alt {
            parts.push("Alt".into());
        }
        if self.shift {
            parts.push("Shift".into());
        }
        if self.win {
            parts.push("Win".into());
        }
        parts.push(match self.trigger {
            Trigger::Key(vk) => key_name(vk),
            Trigger::Mouse(MouseButton::X1) => "Mouse4".into(),
            Trigger::Mouse(MouseButton::X2) => "Mouse5".into(),
            Trigger::Mouse(MouseButton::Middle) => "MouseMiddle".into(),
        });
        write!(f, "{}", parts.join("+"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_legacy_default() {
        let hk = Hotkey::parse("ctrl+shift+`").unwrap().unwrap();
        assert!(hk.ctrl && hk.shift && !hk.alt && !hk.win);
        assert_eq!(hk.trigger, Trigger::Key(0xC0));
        assert_eq!(hk.to_string(), "Ctrl+Shift+`");
        assert_eq!(hk.win32_modifiers(), 2 | 4);
    }

    #[test]
    fn parses_variety() {
        assert_eq!(Hotkey::parse("Alt + F9").unwrap().unwrap().trigger, Trigger::Key(0x78));
        assert_eq!(Hotkey::parse("Ctrl+Num5").unwrap().unwrap().trigger, Trigger::Key(0x65));
        assert_eq!(Hotkey::parse("Ctrl+m").unwrap().unwrap().to_string(), "Ctrl+M");
        assert_eq!(Hotkey::parse("Mouse5").unwrap().unwrap().trigger, Trigger::Mouse(MouseButton::X2));
        assert_eq!(Hotkey::parse("Shift+Mouse4").unwrap().unwrap().to_string(), "Shift+Mouse4");
        assert_eq!(Hotkey::parse("F13").unwrap().unwrap().trigger, Trigger::Key(0x7C));
        assert_eq!(Hotkey::parse("").unwrap(), None);
        assert_eq!(Hotkey::parse("   ").unwrap(), None);
    }

    #[test]
    fn rejects_bad_hotkeys() {
        assert_eq!(Hotkey::parse("Ctrl+Shift"), Err(HotkeyError::NoKey));
        assert_eq!(Hotkey::parse("A"), Err(HotkeyError::NeedsModifier));
        assert_eq!(Hotkey::parse("MouseMiddle"), Err(HotkeyError::NeedsModifier));
        assert!(matches!(Hotkey::parse("Ctrl+Banana"), Err(HotkeyError::UnknownKey(_))));
        assert_eq!(Hotkey::parse("Ctrl+A+B"), Err(HotkeyError::NoKey));
    }

    #[test]
    fn display_roundtrip_all_named_keys() {
        for (name, vk, _) in KEYS {
            let s = format!("Ctrl+{name}");
            let hk = Hotkey::parse(&s).unwrap().unwrap();
            assert_eq!(hk.trigger, Trigger::Key(*vk), "{s}");
            assert_eq!(Hotkey::parse(&hk.to_string()).unwrap().unwrap(), hk);
        }
        for vk in (0x70..=0x87).chain(0x60..=0x69).chain(0x30..=0x39).chain(0x41..=0x5A) {
            let hk = Hotkey { ctrl: true, shift: false, alt: false, win: false, trigger: Trigger::Key(vk) };
            assert_eq!(Hotkey::parse(&hk.to_string()).unwrap().unwrap(), hk);
        }
    }
}

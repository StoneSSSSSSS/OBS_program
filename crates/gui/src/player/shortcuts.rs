//! Player-only keyboard chords. Global recorder hotkeys never use this dispatcher.
use cv_core::config::{HotkeyConfig, PlayerConfig};
use cv_core::hotkey::{self, Hotkey, Trigger};
use eframe::egui::{self, Event, Key, Modifiers};
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Play, Back, Forward, BackLong, ForwardLong, PreviousFrame, NextFrame,
    MarkStart, MarkEnd, VolumeUp, VolumeDown, Mute, Fullscreen, BackOrExit, JumpLatest,
}

pub struct Shortcut {
    pub action: Action,
    pub id: &'static str,
    pub label: &'static str,
    pub defaults: [&'static str; 2],
}

pub const SHORTCUTS: &[Shortcut] = &[
    Shortcut { action: Action::Play, id: "play", label: "Play / pause", defaults: ["Space", "K"] },
    Shortcut { action: Action::Back, id: "seek_back", label: "Seek backward", defaults: ["Left", "J"] },
    Shortcut { action: Action::Forward, id: "seek_forward", label: "Seek forward", defaults: ["Right", "L"] },
    Shortcut { action: Action::BackLong, id: "seek_back_long", label: "Seek backward (large)", defaults: ["Shift+Left", "Shift+J"] },
    Shortcut { action: Action::ForwardLong, id: "seek_forward_long", label: "Seek forward (large)", defaults: ["Shift+Right", "Shift+L"] },
    Shortcut { action: Action::PreviousFrame, id: "previous_frame", label: "Previous frame", defaults: [",", ""] },
    Shortcut { action: Action::NextFrame, id: "next_frame", label: "Next frame", defaults: [".", ""] },
    Shortcut { action: Action::MarkStart, id: "mark_start", label: "Set start", defaults: ["I", ""] },
    Shortcut { action: Action::MarkEnd, id: "mark_end", label: "Set end", defaults: ["O", ""] },
    Shortcut { action: Action::VolumeUp, id: "volume_up", label: "Increase volume", defaults: ["Up", ""] },
    Shortcut { action: Action::VolumeDown, id: "volume_down", label: "Decrease volume", defaults: ["Down", ""] },
    Shortcut { action: Action::Mute, id: "mute", label: "Mute", defaults: ["M", ""] },
    Shortcut { action: Action::Fullscreen, id: "fullscreen", label: "Toggle fullscreen", defaults: ["F", ""] },
    Shortcut { action: Action::JumpLatest, id: "jump_latest", label: "Jump to latest / clip end", defaults: ["End", ""] },
];

impl Shortcut {
    pub fn bindings<'a>(&self, cfg: &'a PlayerConfig) -> [&'a str; 2] {
        cfg.hotkeys.get(self.id).map_or(self.defaults, |keys| [keys[0].as_str(), keys[1].as_str()])
    }
}

pub fn hint(cfg: &PlayerConfig, action: Action, label: &str) -> String {
    let Some(def) = SHORTCUTS.iter().find(|item| item.action == action) else { return label.to_owned(); };
    let mut bindings = def.bindings(cfg).into_iter().map(str::trim).filter(|binding| !binding.is_empty());
    match (bindings.next(), bindings.next()) {
        (Some(primary), Some(alternate)) => format!("{label} ({primary} / {alternate})"),
        (Some(primary), None) => format!("{label} ({primary})"),
        _ => label.to_owned(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Chord {
    key: Key,
    ctrl: bool,
    alt: bool,
    shift: bool,
}

impl Chord {
    fn parse(text: &str) -> Result<Option<Self>, &'static str> {
        if text.trim().is_empty() {
            return Ok(None);
        }
        let (mut ctrl, mut alt, mut shift, mut key) = (false, false, false, None);
        for part in text.split('+').map(str::trim) {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" if !ctrl => ctrl = true,
                "alt" if !alt => alt = true,
                "shift" if !shift => shift = true,
                _ if key.is_none() => {
                    key = parse_key(part);
                    if key.is_none() {
                        return Err("use a key name such as K, Left, F8, Period or Plus");
                    }
                }
                _ => return Err("use one key and optional Ctrl, Alt or Shift modifiers"),
            }
        }
        let key = key.ok_or("a shortcut needs a key")?;
        let binding = Self { key, ctrl, alt, shift };
        if matches!(key, Key::Tab | Key::Escape) {
            return Err("Tab and Esc are reserved for navigation");
        }
        if key == Key::Z && ctrl && !alt && !shift {
            return Err("Ctrl+Z is reserved for undoing audio changes");
        }
        Ok(Some(binding))
    }

    fn from_event(key: Key, modifiers: Modifiers) -> Option<Self> {
        (!modifiers.mac_cmd).then_some(Self {
            key, ctrl: modifiers.ctrl, alt: modifiers.alt, shift: modifiers.shift,
        })
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl { write!(f, "Ctrl+")?; }
        if self.alt { write!(f, "Alt+")?; }
        if self.shift { write!(f, "Shift+")?; }
        write!(f, "{}", self.key.name())
    }
}

fn parse_key(text: &str) -> Option<Key> {
    match text.to_ascii_lowercase().as_str() {
        "left" => Some(Key::ArrowLeft),
        "right" => Some(Key::ArrowRight),
        "up" => Some(Key::ArrowUp),
        "down" => Some(Key::ArrowDown),
        "esc" => Some(Key::Escape),
        _ => Key::from_name(text).or_else(|| Key::ALL.iter().copied().find(|key| key.name().eq_ignore_ascii_case(text))),
    }
}

fn definitions(cfg: &PlayerConfig) -> impl Iterator<Item = (&'static Shortcut, &str)> {
    SHORTCUTS.iter().flat_map(|def| def.bindings(cfg).into_iter().map(move |binding| (def, binding)))
}

pub fn problem(cfg: &PlayerConfig, global: &HotkeyConfig) -> Option<String> {
    let global_bindings = [
        ("Save clip", &global.save_clip),
        ("Save short clip", &global.save_short_clip),
        ("Add marker", &global.add_marker),
        ("Pause/resume 24/7 recording", &global.toggle_recording),
        ("Open ClipVault", &global.open_gui),
    ];
    let global_chords: Vec<_> = global_bindings.into_iter().filter_map(|(label, text)| {
        let hotkey = Hotkey::parse(text).ok().flatten()?;
        let Trigger::Key(vk) = hotkey.trigger else { return None };
        if hotkey.win {
            return None;
        }
        let key = parse_key(&hotkey::key_name(vk))?;
        Some((Chord { key, ctrl: hotkey.ctrl, alt: hotkey.alt, shift: hotkey.shift }, label))
    }).collect();
    let mut used = Vec::<(Chord, &str)>::new();
    for (def, binding) in definitions(cfg) {
        match Chord::parse(binding) {
            Err(error) => return Some(format!("{}: {error}", def.label)),
            Ok(Some(chord)) => {
                if let Some((_, global_action)) = global_chords.iter().find(|(existing, _)| *existing == chord) {
                    return Some(format!("{chord} for {} conflicts with the global {global_action} hotkey", def.label));
                }
                if let Some((_, prior)) = used.iter().find(|(existing, _)| *existing == chord) {
                    return Some(format!("{chord} is assigned to both {prior} and {}", def.label));
                }
                used.push((chord, def.label));
            }
            Ok(None) => {}
        }
    }
    None
}

fn pressed_actions(events: &[Event], cfg: &PlayerConfig) -> Vec<Action> {
    let mut actions = Vec::new();
    for event in events {
        let Event::Key { key, pressed: true, repeat, modifiers, .. } = *event else { continue };
        let Some(chord) = Chord::from_event(key, modifiers) else { continue };
        if key == Key::Escape && !modifiers.any() {
            actions.push(Action::BackOrExit);
            continue;
        }
        let mut matches = definitions(cfg).filter(|(_, text)| Chord::parse(text).ok().flatten() == Some(chord));
        if let Some((def, _)) = matches.next() {
            if matches.next().is_none() && (!repeat || can_repeat(def.action)) && !actions.contains(&def.action) {
                actions.push(def.action);
            }
        }
    }
    actions
}

fn can_repeat(action: Action) -> bool {
    matches!(action, Action::Back | Action::Forward | Action::BackLong | Action::ForwardLong
        | Action::PreviousFrame | Action::NextFrame | Action::VolumeUp | Action::VolumeDown)
}

pub fn actions(ui: &egui::Ui, cfg: &PlayerConfig) -> Vec<Action> {
    if ui.ctx().egui_wants_keyboard_input() {
        return Vec::new();
    }
    ui.input(|i| pressed_actions(&i.events, cfg))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(key: Key, modifiers: Modifiers) -> Event {
        Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
    }

    #[test]
    fn defaults_preserve_aliases_with_exact_modifiers() {
        let cfg = PlayerConfig::default();
        assert_eq!(problem(&cfg, &HotkeyConfig::default()), None);
        assert_eq!(pressed_actions(&[event(Key::J, Modifiers::SHIFT)], &cfg), [Action::BackLong]);
        assert_eq!(pressed_actions(&[event(Key::J, Modifiers::CTRL)], &cfg), []);
        assert_eq!(pressed_actions(&[event(Key::Comma, Modifiers::NONE)], &cfg), [Action::PreviousFrame]);
        assert_eq!(pressed_actions(&[event(Key::End, Modifiers::NONE)], &cfg), [Action::JumpLatest]);
    }

    #[test]
    fn remapping_replaces_both_defaults_and_conflicts_do_not_dispatch() {
        let mut cfg = PlayerConfig::default();
        cfg.hotkeys.insert("next_frame".into(), ["Ctrl+F8".into(), String::new()]);
        assert_eq!(pressed_actions(&[event(Key::Period, Modifiers::NONE)], &cfg), []);
        assert_eq!(pressed_actions(&[event(Key::F8, Modifiers::CTRL)], &cfg), [Action::NextFrame]);
        cfg.hotkeys.insert("jump_latest".into(), ["Control+f8".into(), String::new()]);
        assert!(problem(&cfg, &HotkeyConfig::default()).is_some());
        assert_eq!(pressed_actions(&[event(Key::F8, Modifiers::CTRL)], &cfg), []);
    }

    #[test]
    fn rejects_global_keyboard_collisions_and_reserved_audio_undo() {
        let mut cfg = PlayerConfig::default();
        let global = HotkeyConfig::default();
        cfg.hotkeys.insert("mute".into(), ["Ctrl+Shift+M".into(), String::new()]);
        assert!(problem(&cfg, &global).unwrap().contains("global Add marker"));
        cfg.hotkeys.insert("mute".into(), ["Ctrl+Z".into(), String::new()]);
        assert!(problem(&cfg, &global).unwrap().contains("reserved for undoing audio changes"));
    }

    #[test]
    fn text_edit_focus_blocks_player_actions() {
        let ctx = egui::Context::default();
        let cfg = PlayerConfig::default();
        let mut name = String::new();
        let mut initial = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.text_edit_singleline(&mut name).request_focus();
        });
        initial.textures_delta.clear();
        let mut output = ctx.run_ui(egui::RawInput {
            events: vec![Event::Text("k".into()), event(Key::K, Modifiers::NONE)],
            ..Default::default()
        }, |ui| {
            assert!(actions(ui, &cfg).is_empty());
            ui.text_edit_singleline(&mut name);
        });
        output.textures_delta.clear();
        assert_eq!(name, "k");
    }
}

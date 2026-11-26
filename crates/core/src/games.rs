//! Game detection.
//!
//! The old program named clips after the *window title* at the moment the
//! hotkey was pressed. Titles change constantly (Chrome tabs, Discord
//! channels, anti-cheat obfuscation), so clips were scattered across dozens of
//! folders. We instead identify the *executable*, classify whether it is a
//! game, and attribute a clip to the game that was in focus the most during
//! the clip's time window.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Information about a foreground window sample.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub exe_path: String,
    pub exe_name: String,
    pub title: String,
    pub class: String,
    pub pid: u32,
    pub fullscreen: bool,
}

/// Programs that are never games, even when full-screen.
pub const NON_GAMES: &[&str] = &[
    "explorer.exe", "searchhost.exe", "searchapp.exe", "startmenuexperiencehost.exe",
    "shellexperiencehost.exe", "applicationframehost.exe", "lockapp.exe", "taskmgr.exe",
    "systemsettings.exe", "textinputhost.exe", "dwm.exe", "rundll32.exe", "mmc.exe",
    "chrome.exe", "msedge.exe", "firefox.exe", "opera.exe", "opera_gx.exe", "brave.exe",
    "vivaldi.exe", "iexplore.exe", "discord.exe", "discordptb.exe", "discordcanary.exe",
    "slack.exe", "teams.exe", "ms-teams.exe", "zoom.exe", "skype.exe", "telegram.exe",
    "whatsapp.exe", "spotify.exe", "vlc.exe", "mpv.exe", "mpc-hc64.exe", "potplayermini64.exe",
    "wmplayer.exe", "music.ui.exe", "video.ui.exe", "obs64.exe", "obs32.exe", "clipvault.exe",
    "clipvault-service.exe", "medal.exe", "steam.exe", "steamwebhelper.exe", "epicgameslauncher.exe",
    "riotclientservices.exe", "riotclientux.exe", "riotclientuxrender.exe", "battle.net.exe",
    "eadesktop.exe", "ealauncher.exe", "origin.exe", "upc.exe", "ubisoftconnect.exe",
    "galaxyclient.exe", "playnite.desktopapp.exe", "playnite.fullscreenapp.exe", "code.exe",
    "devenv.exe", "idea64.exe", "pycharm64.exe", "clion64.exe", "rider64.exe", "rustrover64.exe",
    "jetbrains-toolbox.exe", "notepad.exe", "notepad++.exe", "windowsterminal.exe", "cmd.exe",
    "powershell.exe", "pwsh.exe", "conhost.exe", "winword.exe", "excel.exe", "powerpnt.exe",
    "outlook.exe", "onenote.exe", "acrobat.exe", "acrord32.exe", "photoshop.exe", "blender.exe",
    "unity.exe", "unityhub.exe", "unrealeditor.exe", "nvidia app.exe", "nvcontainer.exe",
    "msiafterburner.exe", "realtemp.exe", "shotcut.exe", "resolve.exe", "premiere pro.exe",
    "vmware.exe", "virtualbox.exe", "mstsc.exe", "nordvpn.exe", "voicemeeter.exe",
    "voicemeeterpro.exe", "voicemeeter8.exe", "oculusclient.exe", "virtual desktop.exe",
];

/// Executables that are games even though they live outside game folders.
const KNOWN_GAMES: &[(&str, &str)] = &[
    ("robloxplayerbeta.exe", "Roblox"),
    ("minecraft.windows.exe", "Minecraft"),
    ("league of legends.exe", "League of Legends"),
    ("valorant-win64-shipping.exe", "VALORANT"),
    ("cs2.exe", "Counter-Strike 2"),
    ("csgo.exe", "Counter-Strike: Global Offensive"),
    ("dota2.exe", "Dota 2"),
    ("fortniteclient-win64-shipping.exe", "Fortnite"),
    ("r5apex.exe", "Apex Legends"),
    ("r5apex_dx12.exe", "Apex Legends"),
    ("overwatch.exe", "Overwatch 2"),
    ("rainbowsix.exe", "Rainbow Six Siege"),
    ("rainbowsix_vulkan.exe", "Rainbow Six Siege"),
    ("rainbowsix_be.exe", "Rainbow Six Siege"),
    ("deadlock.exe", "Deadlock"),
    ("project8.exe", "Deadlock"),
    ("rotmg exalt.exe", "Realm of the Mad God Exalt"),
    ("bf2042.exe", "Battlefield 2042"),
    ("bfv.exe", "Battlefield V"),
    ("gta5.exe", "Grand Theft Auto V"),
    ("eldenring.exe", "Elden Ring"),
    ("rocketleague.exe", "Rocket League"),
];

/// Folder fragments that indicate an installed game.
const GAME_PATH_MARKERS: &[&str] = &[
    r"\steamapps\common\",
    r"\epic games\",
    r"\riot games\",
    r"\gog galaxy\games\",
    r"\gog games\",
    r"\ubisoft game launcher\games\",
    r"\ea games\",
    r"\origin games\",
    r"\xboxgames\",
    r"\battle.net\",
    r"\blizzard\",
    r"\rockstar games\",
    r"\itch\apps\",
    r"\roblox\versions\",
    r"\games\",
];

/// Lower-case exe file name from a path.
pub fn exe_name_of(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|f| f.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

/// Stable key for a game. Java games are keyed by title since they all run as
/// `javaw.exe` (Minecraft Java Edition being the common case).
pub fn game_key(info: &WindowInfo) -> String {
    let exe = if info.exe_name.is_empty() { exe_name_of(&info.exe_path) } else { info.exe_name.to_lowercase() };
    if exe == "javaw.exe" || exe == "java.exe" {
        let title = crate::naming::strip_invisible(&info.title);
        if title.to_lowercase().starts_with("minecraft") {
            return "java:minecraft".into();
        }
    }
    exe
}

/// Why something was (or wasn't) considered a game; useful for the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    UserGame,
    UserNotGame,
    KnownGame,
    GameFolder,
    Fullscreen,
    KnownNonGame,
    NotGame,
}

impl Verdict {
    pub fn is_game(self) -> bool {
        matches!(self, Verdict::UserGame | Verdict::KnownGame | Verdict::GameFolder | Verdict::Fullscreen)
    }
}

pub struct Classifier {
    always: Vec<String>,
    never: Vec<String>,
    library_folders: Vec<String>,
    fullscreen_is_game: bool,
    /// Per-exe overrides stored in the database (Games page toggles).
    db_overrides: HashMap<String, bool>,
}

impl Classifier {
    pub fn new(cfg: &crate::config::GamesConfig, db_overrides: HashMap<String, bool>) -> Self {
        let norm = |v: &Vec<String>| v.iter().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty()).collect();
        Self {
            always: norm(&cfg.always_games),
            never: norm(&cfg.never_games),
            library_folders: cfg
                .library_folders
                .iter()
                .map(|s| {
                    let mut p = s.trim().to_lowercase().replace('/', "\\");
                    if !p.ends_with('\\') {
                        p.push('\\');
                    }
                    p
                })
                .filter(|s| s.len() > 1)
                .collect(),
            fullscreen_is_game: cfg.fullscreen_is_game,
            db_overrides,
        }
    }

    pub fn classify(&self, info: &WindowInfo) -> Verdict {
        let key = game_key(info);
        let exe = if info.exe_name.is_empty() { exe_name_of(&info.exe_path) } else { info.exe_name.to_lowercase() };
        if exe.is_empty() {
            return Verdict::NotGame;
        }
        if self.never.contains(&exe) || self.never.contains(&key) {
            return Verdict::UserNotGame;
        }
        if self.always.contains(&exe) || self.always.contains(&key) {
            return Verdict::UserGame;
        }
        if let Some(&is_game) = self.db_overrides.get(&key).or_else(|| self.db_overrides.get(&exe)) {
            return if is_game { Verdict::UserGame } else { Verdict::UserNotGame };
        }
        if key == "java:minecraft" || KNOWN_GAMES.iter().any(|(e, _)| *e == exe) {
            return Verdict::KnownGame;
        }
        if NON_GAMES.contains(&exe.as_str()) {
            return Verdict::KnownNonGame;
        }
        let path = info.exe_path.to_lowercase().replace('/', "\\");
        if !path.is_empty() {
            if self.library_folders.iter().any(|f| path.starts_with(f.as_str())) {
                return Verdict::GameFolder;
            }
            if GAME_PATH_MARKERS.iter().any(|m| path.contains(m)) && !is_launcher_like(&exe) {
                return Verdict::GameFolder;
            }
            if path.starts_with(r"c:\windows\") {
                return Verdict::NotGame;
            }
        }
        if self.fullscreen_is_game && info.fullscreen {
            return Verdict::Fullscreen;
        }
        Verdict::NotGame
    }
}

fn is_launcher_like(exe: &str) -> bool {
    let stem = exe.trim_end_matches(".exe");
    ["launcher", "crashreport", "crashhandler", "updater", "setup", "unins", "helper", "bootstrap", "redist", "vc_redist", "dxsetup", "easyanticheat", "battleye", "cefprocess", "webhelper"]
        .iter()
        .any(|w| stem.contains(w))
}

/// Built-in friendly name for well-known exes.
pub fn known_game_name(key: &str) -> Option<&'static str> {
    if key == "java:minecraft" {
        return Some("Minecraft");
    }
    KNOWN_GAMES.iter().find(|(e, _)| *e == key).map(|(_, n)| *n)
}

/// Best-effort friendly name. Inputs in order of trust: Steam manifest name,
/// built-in table, exe version-info product name/description, then the exe stem.
pub fn friendly_name(
    key: &str,
    exe_path: &str,
    steam_name: Option<&str>,
    product_name: Option<&str>,
    file_description: Option<&str>,
) -> String {
    let clean = |s: &str| crate::naming::collapse_ws(&crate::naming::strip_invisible(s));
    if let Some(n) = steam_name.map(clean).filter(|s| !s.is_empty()) {
        return n;
    }
    if let Some(n) = known_game_name(key) {
        return n.to_string();
    }
    let generic = |s: &str| {
        let l = s.to_lowercase();
        l.is_empty()
            || l.contains("unreal engine")
            || l.contains("unity")
            || l == "game"
            || l.contains("bootstrap")
            || l.contains("launcher")
            || l.starts_with("microsoft")
            || l == "java(tm) platform se binary"
            || l == "openjdk platform binary"
    };
    for cand in [product_name, file_description].into_iter().flatten() {
        let c = clean(cand);
        if !generic(&c) && c.chars().any(|ch| ch.is_alphabetic()) && c.chars().count() <= 60 {
            return c;
        }
    }
    // Exe stem: strip common suffixes like "-Win64-Shipping".
    let stem = Path::new(exe_path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| key.trim_end_matches(".exe").to_string());
    let mut stem = stem;
    for suf in ["-Win64-Shipping", "-WinGDK-Shipping", "_x64", "-Win64", "64", "_dx12", "_dx11", "_vulkan", "_BE", "_EAC"] {
        if stem.len() > suf.len() && stem.to_lowercase().ends_with(&suf.to_lowercase()) {
            stem.truncate(stem.len() - suf.len());
        }
    }
    let stem = stem.replace(['_', '-'], " ");
    let stem = crate::naming::collapse_ws(&stem);
    if stem.is_empty() {
        "Unknown".into()
    } else {
        // Capitalise the first letter of each word if the whole thing is lower case.
        if stem.chars().all(|c| !c.is_uppercase()) {
            stem.split(' ')
                .map(|w| {
                    let mut c = w.chars();
                    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join(" ")
        } else {
            stem
        }
    }
}

/// Parse a Steam `appmanifest_*.acf` file: (appid, name, installdir).
pub fn parse_acf(text: &str) -> Option<(String, String, String)> {
    let mut appid = None;
    let mut name = None;
    let mut installdir = None;
    let mut depth = 0i32;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('{') {
            depth += 1;
            continue;
        }
        if t.starts_with('}') {
            depth -= 1;
            continue;
        }
        if depth != 1 {
            continue;
        }
        let parts: Vec<&str> = t.split('"').collect();
        // "key"  "value"  ->  ["", key, ws, value, ""]
        if parts.len() >= 5 {
            let key = parts[1].to_ascii_lowercase();
            let value = parts[3].replace("\\\\", "\\");
            match key.as_str() {
                "appid" => appid = Some(value),
                "name" => name = Some(value),
                "installdir" => installdir = Some(value),
                _ => {}
            }
        }
    }
    Some((appid?, name?, installdir?))
}

/// For an exe inside `<lib>\steamapps\common\<installdir>\...`, return
/// (`<lib>\steamapps`, `<installdir>`).
pub fn steam_install_of(exe_path: &str) -> Option<(PathBuf, String)> {
    let lower = exe_path.to_lowercase().replace('/', "\\");
    let marker = r"\steamapps\common\";
    let idx = lower.find(marker)?;
    let steamapps = PathBuf::from(&exe_path[..idx + r"\steamapps".len()]);
    let rest = &exe_path[idx + marker.len()..];
    let installdir = rest.split(['\\', '/']).next()?.to_string();
    if installdir.is_empty() {
        return None;
    }
    Some((steamapps, installdir))
}

/// Look up the Steam store name for an exe (reads appmanifest files).
pub fn steam_name_for_exe(exe_path: &str) -> Option<String> {
    let (steamapps, installdir) = steam_install_of(exe_path)?;
    let entries = std::fs::read_dir(&steamapps).ok()?;
    for e in entries.flatten() {
        let fname = e.file_name().to_string_lossy().to_lowercase();
        if !(fname.starts_with("appmanifest_") && fname.ends_with(".acf")) {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(e.path()) {
            if let Some((_, name, dir)) = parse_acf(&text) {
                if dir.eq_ignore_ascii_case(&installdir) {
                    return Some(name);
                }
            }
        }
    }
    None
}

/// A span of foreground activity (from the activity table).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub key: String,
    pub is_game: bool,
    pub start_ms: i64,
    pub end_ms: i64,
}

/// Work out which game (or app) a clip covering `[from_ms, to_ms]` belongs to.
///
/// Games beat non-games; among games the one with the most focused time wins,
/// with the last 25% of the window counting double (that's usually what you
/// just clipped). Returns `(key, is_game)`.
pub fn attribute(spans: &[Span], from_ms: i64, to_ms: i64) -> Option<(String, bool)> {
    if to_ms <= from_ms {
        return None;
    }
    let recent_from = to_ms - (to_ms - from_ms) / 4;
    let mut game_time: HashMap<&str, i64> = HashMap::new();
    let mut app_time: HashMap<&str, i64> = HashMap::new();
    for s in spans {
        let a = s.start_ms.max(from_ms);
        let b = s.end_ms.min(to_ms);
        if b <= a {
            continue;
        }
        let recent = (b.min(to_ms) - a.max(recent_from)).max(0);
        let weight = (b - a) + recent;
        let map = if s.is_game { &mut game_time } else { &mut app_time };
        *map.entry(s.key.as_str()).or_default() += weight;
    }
    let best = |m: &HashMap<&str, i64>| {
        m.iter()
            .max_by(|a, b| a.1.cmp(b.1).then(b.0.cmp(a.0)))
            .map(|(k, _)| k.to_string())
    };
    if let Some(k) = best(&game_time) {
        return Some((k, true));
    }
    best(&app_time).map(|k| (k, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GamesConfig;

    fn win(path: &str, title: &str, fullscreen: bool) -> WindowInfo {
        WindowInfo {
            exe_path: path.into(),
            exe_name: exe_name_of(path),
            title: title.into(),
            class: String::new(),
            pid: 1,
            fullscreen,
        }
    }

    fn classifier() -> Classifier {
        Classifier::new(&GamesConfig::default(), HashMap::new())
    }

    #[test]
    fn classifies_common_cases() {
        let c = classifier();
        assert_eq!(c.classify(&win(r"C:\Program Files (x86)\Steam\steamapps\common\Deadlock\game\bin\win64\project8.exe", "Deadlock", true)), Verdict::KnownGame);
        assert_eq!(c.classify(&win(r"D:\SteamLibrary\steamapps\common\Domain of Magica\DomainOfMagica.exe", "Domain of Magica", false)), Verdict::GameFolder);
        assert_eq!(c.classify(&win(r"C:\Program Files\Google\Chrome\Application\chrome.exe", "YouTube", true)), Verdict::KnownNonGame);
        assert_eq!(c.classify(&win(r"C:\Users\me\AppData\Local\Discord\app-1.0\Discord.exe", "#general", false)), Verdict::KnownNonGame);
        assert_eq!(c.classify(&win(r"C:\Tools\SomeTool.exe", "Tool", false)), Verdict::NotGame);
        assert_eq!(c.classify(&win(r"C:\Indie\Cool.exe", "Cool", true)), Verdict::Fullscreen);
        assert_eq!(c.classify(&win(r"E:\games\Riot Games\Riot Client\RiotClientServices.exe", "Riot", false)), Verdict::KnownNonGame);
        assert_eq!(c.classify(&win(r"E:\games\Something\Launcher.exe", "x", false)), Verdict::NotGame);
        assert_eq!(c.classify(&win(r"C:\Windows\System32\notepad.exe", "x", true)), Verdict::KnownNonGame);
        assert!(c.classify(&win(r"C:\Program Files\Java\bin\javaw.exe", "Minecraft 1.20.1", false)).is_game());
    }

    #[test]
    fn user_overrides_win() {
        let cfg = GamesConfig {
            always_games: vec!["SomeTool.exe".into()],
            never_games: vec!["Cool.exe".into()],
            library_folders: vec![r"X:\MyGames".into()],
            ..GamesConfig::default()
        };
        let mut db = HashMap::new();
        db.insert("chrome.exe".to_string(), true);
        let c = Classifier::new(&cfg, db);
        assert_eq!(c.classify(&win(r"C:\Tools\SomeTool.exe", "Tool", false)), Verdict::UserGame);
        assert_eq!(c.classify(&win(r"C:\Indie\Cool.exe", "Cool", true)), Verdict::UserNotGame);
        assert_eq!(c.classify(&win(r"X:\MyGames\thing\run.exe", "t", false)), Verdict::GameFolder);
        assert_eq!(c.classify(&win(r"C:\Chrome\chrome.exe", "t", false)), Verdict::UserGame);
    }

    #[test]
    fn keys_and_names() {
        assert_eq!(game_key(&win(r"C:\x\javaw.exe", "Minecraft* 1.21 - Multiplayer", false)), "java:minecraft");
        assert_eq!(game_key(&win(r"C:\x\Deadlock.EXE", "", false)), "deadlock.exe");
        assert_eq!(friendly_name("java:minecraft", r"C:\x\javaw.exe", None, Some("Java(TM) Platform SE binary"), None), "Minecraft");
        assert_eq!(friendly_name("x.exe", r"C:\x\x.exe", Some("Counter-Strike 2"), None, None), "Counter-Strike 2");
        assert_eq!(friendly_name("valorant-win64-shipping.exe", r"C:\VALORANT-Win64-Shipping.exe", None, None, None), "VALORANT");
        assert_eq!(friendly_name("foo-win64-shipping.exe", r"C:\Foo-Win64-Shipping.exe", None, Some("Unreal Engine"), None), "Foo");
        assert_eq!(friendly_name("domainofmagica.exe", r"C:\DomainOfMagica.exe", None, Some("Domain of Magica"), None), "Domain of Magica");
        assert_eq!(friendly_name("noita.exe", r"C:\noita.exe", None, None, None), "Noita");
    }

    #[test]
    fn acf_parsing_and_steam_paths() {
        let acf = "\"AppState\"\n{\n\t\"appid\"\t\t\"730\"\n\t\"name\"\t\t\"Counter-Strike 2\"\n\t\"installdir\"\t\t\"Counter-Strike Global Offensive\"\n\t\"UserConfig\"\n\t{\n\t\t\"name\"\t\"ignored\"\n\t}\n}\n";
        assert_eq!(
            parse_acf(acf),
            Some(("730".into(), "Counter-Strike 2".into(), "Counter-Strike Global Offensive".into()))
        );
        let (lib, dir) = steam_install_of(r"D:\SteamLibrary\steamapps\common\Counter-Strike Global Offensive\game\bin\win64\cs2.exe").unwrap();
        assert_eq!(lib, PathBuf::from(r"D:\SteamLibrary\steamapps"));
        assert_eq!(dir, "Counter-Strike Global Offensive");
        assert!(steam_install_of(r"C:\Games\x.exe").is_none());
    }

    #[test]
    fn steam_manifest_lookup_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let steamapps = tmp.path().join("steamapps");
        let game_dir = steamapps.join("common").join("My Game").join("bin");
        std::fs::create_dir_all(&game_dir).unwrap();
        std::fs::write(
            steamapps.join("appmanifest_123.acf"),
            "\"AppState\"\n{\n\"appid\" \"123\"\n\"name\" \"My Great Game\"\n\"installdir\" \"My Game\"\n}\n",
        )
        .unwrap();
        let exe = game_dir.join("game.exe");
        assert_eq!(steam_name_for_exe(&exe.to_string_lossy()).as_deref(), Some("My Great Game"));
    }

    #[test]
    fn attribution_prefers_games_and_recent_time() {
        let s = |k: &str, g: bool, a: i64, b: i64| Span { key: k.into(), is_game: g, start_ms: a, end_ms: b };
        // 120s clip: Discord focused 70s, game 50s -> game wins (games beat apps).
        let spans = vec![s("deadlock.exe", true, 0, 50_000), s("discord.exe", false, 50_000, 120_000)];
        assert_eq!(attribute(&spans, 0, 120_000), Some(("deadlock.exe".into(), true)));
        // Two games: the one played at the end gets the recency bonus.
        let spans = vec![s("a.exe", true, 0, 65_000), s("b.exe", true, 65_000, 120_000)];
        assert_eq!(attribute(&spans, 0, 120_000), Some(("b.exe".into(), true)));
        // No games: dominant app.
        let spans = vec![s("chrome.exe", false, 0, 100_000), s("discord.exe", false, 100_000, 120_000)];
        assert_eq!(attribute(&spans, 0, 120_000), Some(("chrome.exe".into(), false)));
        // Outside the window / empty.
        assert_eq!(attribute(&spans, 200_000, 300_000), None);
        assert_eq!(attribute(&[], 0, 10), None);
    }
}

//! Tracks which app/game is in the foreground, second by second.
//!
//! This history powers: naming clips after the right game, the game bands on
//! the 24/7 timeline, and pointing the "Game" audio track at the game.

use crate::shared::Shared;
use crate::win::foreground::{self, ExeCache};
use cv_core::db::Db;
use cv_core::games::{self, Classifier, WindowInfo};
use cv_core::util::now_ms;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct OpenSpan {
    key: String,
    id: i64,
    last_seen: i64,
    last_flush: i64,
    title: String,
}

struct GameCacheEntry {
    id: i64,
    name: String,
    is_game: bool,
    refreshed: Instant,
}

pub fn run(sh: Arc<Shared>) {
    let db = match Db::open_default() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("activity tracker: cannot open database: {e:#}");
            return;
        }
    };
    let mut exe_cache = ExeCache::default();
    let mut games_cache: HashMap<String, GameCacheEntry> = HashMap::new();
    let mut classifier = make_classifier(&sh, &db);
    let mut classifier_built = Instant::now();
    let mut span: Option<OpenSpan> = None;
    let mut last_game_name: Option<(String, Instant)> = None;
    let mut last_prune = Instant::now() - Duration::from_secs(3600);

    while !sh.is_shutting_down() {
        let cfg = sh.cfg();
        std::thread::sleep(Duration::from_millis(cfg.advanced.activity_sample_ms as u64));
        let now = now_ms();
        if classifier_built.elapsed() > Duration::from_secs(30) {
            classifier = make_classifier(&sh, &db);
            classifier_built = Instant::now();
            games_cache.retain(|_, g| g.refreshed.elapsed() < Duration::from_secs(600));
        }

        let fg = foreground::foreground(&mut exe_cache);
        let Some((info, _hwnd)) = fg else {
            // Nothing focused (desktop/lock screen): close the span after a grace period.
            if let Some(s) = &span {
                if now - s.last_seen > 3000 {
                    let _ = db.extend_activity(s.id, s.last_seen, None);
                    span = None;
                }
            }
            continue;
        };
        if info.exe_path.is_empty() {
            continue; // elevated/protected process we can't inspect
        }
        let key = games::game_key(&info);
        let verdict = classifier.classify(&info);
        let is_game = verdict.is_game();

        // Resolve the game row (cached).
        let entry = match games_cache.get(&key) {
            Some(e) if e.is_game == is_game => e,
            _ => {
                let name = resolve_name(&key, &info);
                match db.upsert_game(&key, &name, &info.exe_path, is_game, now) {
                    Ok(id) => {
                        let name = db.game(id).ok().flatten().map(|g| g.name).unwrap_or(name);
                        games_cache.insert(key.clone(), GameCacheEntry { id, name, is_game, refreshed: Instant::now() });
                    }
                    Err(e) => {
                        tracing::warn!("activity db: {e:#}");
                        continue;
                    }
                }
                games_cache.get(&key).unwrap()
            }
        };
        let (game_id, game_name) = (entry.id, entry.name.clone());

        // Extend or start the activity span.
        let title = cv_core::naming::strip_invisible(&info.title);
        match &mut span {
            Some(s) if s.key == key && now - s.last_seen < 5000 => {
                s.last_seen = now;
                if now - s.last_flush > 10_000 || s.title != title {
                    let t = if s.title != title { Some(title.as_str()) } else { None };
                    let _ = db.extend_activity(s.id, now, t);
                    s.last_flush = now;
                    s.title = title.clone();
                }
            }
            _ => {
                if let Some(s) = &span {
                    let _ = db.extend_activity(s.id, s.last_seen, None);
                }
                match db.start_activity(&key, Some(game_id), is_game, &title, now) {
                    Ok(id) => span = Some(OpenSpan { key: key.clone(), id, last_seen: now, last_flush: now, title }),
                    Err(e) => tracing::warn!("activity db: {e:#}"),
                }
            }
        }

        // Status: current game (sticky for 5 minutes after leaving it).
        if is_game {
            last_game_name = Some((game_name.clone(), Instant::now()));
        }
        let current = last_game_name
            .as_ref()
            .filter(|(_, t)| t.elapsed() < Duration::from_secs(300))
            .map(|(n, _)| n.clone());
        sh.update_status(|s| s.game.current = current);

        if last_prune.elapsed() > Duration::from_secs(3600) {
            last_prune = Instant::now();
            let keep_days = cfg.advanced.activity_history_days.max(cfg.recording.retention_hours.div_ceil(24) + 1);
            let _ = db.prune_activity(now - keep_days as i64 * 86_400_000);
            let _ = db.prune_markers(now - keep_days as i64 * 86_400_000);
        }
    }
    if let Some(s) = span {
        let _ = db.extend_activity(s.id, s.last_seen, None);
    }
}

fn make_classifier(sh: &Shared, db: &Db) -> Classifier {
    let overrides = db.game_overrides().unwrap_or_default();
    Classifier::new(&sh.cfg.read().games, overrides)
}

fn resolve_name(key: &str, info: &WindowInfo) -> String {
    let steam = games::steam_name_for_exe(&info.exe_path);
    let (product, desc) = foreground::version_strings(&info.exe_path);
    games::friendly_name(key, &info.exe_path, steam.as_deref(), product.as_deref(), desc.as_deref())
}

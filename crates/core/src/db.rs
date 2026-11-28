//! SQLite library database (`%LOCALAPPDATA%\ClipVault\library.db`).
//!
//! Opened by both the service (writer of most data) and the GUI (reads, plus
//! user edits). WAL mode + a busy timeout make that safe.

use crate::model::*;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::collections::HashMap;
use std::path::Path;

const SCHEMA_VERSION: i64 = 2;

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS games (
    id INTEGER PRIMARY KEY,
    key TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    exe_path TEXT NOT NULL DEFAULT '',
    is_game INTEGER,
    auto_is_game INTEGER NOT NULL DEFAULT 0,
    name_locked INTEGER NOT NULL DEFAULT 0,
    first_seen INTEGER NOT NULL,
    last_seen INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS categories (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE COLLATE NOCASE,
    color TEXT NOT NULL DEFAULT '',
    sort INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS clips (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE COLLATE NOCASE,
    title TEXT NOT NULL,
    game_id INTEGER REFERENCES games(id) ON DELETE SET NULL,
    created_at INTEGER NOT NULL,
    source_start_ms INTEGER,
    duration REAL NOT NULL DEFAULT 0,
    size INTEGER NOT NULL DEFAULT 0,
    width INTEGER NOT NULL DEFAULT 0,
    height INTEGER NOT NULL DEFAULT 0,
    fps REAL NOT NULL DEFAULT 0,
    audio_tracks INTEGER NOT NULL DEFAULT 0,
    track_names TEXT NOT NULL DEFAULT '[]',
    favorite INTEGER NOT NULL DEFAULT 0,
    notes TEXT NOT NULL DEFAULT '',
    source TEXT NOT NULL DEFAULT 'replay',
    mixer TEXT NOT NULL DEFAULT '',
    thumb TEXT NOT NULL DEFAULT '',
    probed INTEGER NOT NULL DEFAULT 0,
    missing INTEGER NOT NULL DEFAULT 0,
    last_position REAL NOT NULL DEFAULT 0,
    views INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS clips_game ON clips(game_id);
CREATE INDEX IF NOT EXISTS clips_created ON clips(created_at);
CREATE TABLE IF NOT EXISTS clip_categories (
    clip_id INTEGER NOT NULL REFERENCES clips(id) ON DELETE CASCADE,
    category_id INTEGER NOT NULL REFERENCES categories(id) ON DELETE CASCADE,
    PRIMARY KEY (clip_id, category_id)
);
CREATE TABLE IF NOT EXISTS recordings (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL UNIQUE COLLATE NOCASE,
    start_ms INTEGER NOT NULL,
    duration REAL NOT NULL DEFAULT 0,
    size INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'complete',
    audio_tracks INTEGER NOT NULL DEFAULT 0,
    kept INTEGER NOT NULL DEFAULT 0,
    probed INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS recordings_start ON recordings(start_ms);
CREATE TABLE IF NOT EXISTS activity (
    id INTEGER PRIMARY KEY,
    key TEXT NOT NULL,
    game_id INTEGER REFERENCES games(id) ON DELETE SET NULL,
    is_game INTEGER NOT NULL DEFAULT 0,
    title TEXT NOT NULL DEFAULT '',
    start_ms INTEGER NOT NULL,
    end_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS activity_end ON activity(end_ms);
CREATE TABLE IF NOT EXISTS markers (
    id INTEGER PRIMARY KEY,
    at_ms INTEGER NOT NULL,
    label TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS markers_at ON markers(at_ms);
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY,
    at_ms INTEGER NOT NULL,
    level TEXT NOT NULL,
    message TEXT NOT NULL
);
"#;

pub struct Db {
    conn: Connection,
}

/// Clip mixer column: current format (MixSettings JSON) or the old list of
/// per-track gains.
fn parse_mix(s: &str) -> Option<crate::mixer::MixSettings> {
    if s.trim().is_empty() {
        return None;
    }
    if let Ok(m) = serde_json::from_str::<crate::mixer::MixSettings>(s) {
        return Some(m);
    }
    serde_json::from_str::<Vec<f32>>(s).ok().map(|g| crate::mixer::MixSettings::from_legacy_gains(&g))
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    pub fn open_default() -> Result<Db> {
        Self::open(&crate::paths::db_path())
    }

    pub fn open_in_memory() -> Result<Db> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Db> {
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Db { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(SCHEMA_V1)?;
        // v2: non-destructive audio edits per clip.
        let has_edits: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('clips') WHERE name='audio_edits'",
            [],
            |r| r.get(0),
        )?;
        if has_edits == 0 {
            self.conn.execute("ALTER TABLE clips ADD COLUMN audio_edits TEXT NOT NULL DEFAULT ''", [])?;
        }
        let v: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| r.get(0))
            .optional()?;
        let v: i64 = v.and_then(|s| s.parse().ok()).unwrap_or(0);
        if v > SCHEMA_VERSION {
            anyhow::bail!("library.db was created by a newer ClipVault (schema {v})");
        }
        self.conn.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES ('schema_version', ?1)",
            [SCHEMA_VERSION.to_string()],
        )?;
        Ok(())
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute("INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)", [key, value])?;
        Ok(())
    }

    // ------------------------------------------------------------- games

    fn row_game(r: &Row) -> rusqlite::Result<Game> {
        let ov: Option<i64> = r.get("is_game")?;
        Ok(Game {
            id: r.get("id")?,
            key: r.get("key")?,
            name: r.get("name")?,
            exe_path: r.get("exe_path")?,
            is_game_override: ov.map(|v| v != 0),
            auto_is_game: r.get::<_, i64>("auto_is_game")? != 0,
            name_locked: r.get::<_, i64>("name_locked")? != 0,
            first_seen: r.get("first_seen")?,
            last_seen: r.get("last_seen")?,
            clip_count: r.get("clip_count").unwrap_or(0),
        })
    }

    /// Record that `key` was seen; creates it if new. Name is only updated if
    /// the user hasn't renamed the game. Returns the game id.
    pub fn upsert_game(&self, key: &str, name: &str, exe_path: &str, auto_is_game: bool, now: i64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO games(key, name, exe_path, auto_is_game, first_seen, last_seen)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(key) DO UPDATE SET
                name = CASE WHEN games.name_locked = 1 THEN games.name ELSE excluded.name END,
                exe_path = CASE WHEN excluded.exe_path <> '' THEN excluded.exe_path ELSE games.exe_path END,
                auto_is_game = MAX(games.auto_is_game, excluded.auto_is_game),
                last_seen = MAX(games.last_seen, excluded.last_seen)",
            params![key, name, exe_path, auto_is_game as i64, now],
        )?;
        Ok(self.conn.query_row("SELECT id FROM games WHERE key=?1", [key], |r| r.get(0))?)
    }

    pub fn game(&self, id: i64) -> Result<Option<Game>> {
        Ok(self
            .conn
            .query_row(
                "SELECT g.*, (SELECT COUNT(*) FROM clips c WHERE c.game_id=g.id AND c.missing=0) AS clip_count
                 FROM games g WHERE g.id=?1",
                [id],
                Self::row_game,
            )
            .optional()?)
    }

    pub fn game_by_key(&self, key: &str) -> Result<Option<Game>> {
        Ok(self
            .conn
            .query_row("SELECT g.*, 0 AS clip_count FROM games g WHERE g.key=?1", [key], Self::row_game)
            .optional()?)
    }

    /// Find a game by display name (case-insensitive).
    pub fn game_by_name(&self, name: &str) -> Result<Option<Game>> {
        Ok(self
            .conn
            .query_row(
                "SELECT g.*, 0 AS clip_count FROM games g WHERE g.name = ?1 COLLATE NOCASE ORDER BY g.id LIMIT 1",
                [name],
                Self::row_game,
            )
            .optional()?)
    }

    /// All games/apps, most clips first.
    pub fn list_games(&self) -> Result<Vec<Game>> {
        let mut st = self.conn.prepare(
            "SELECT g.*, (SELECT COUNT(*) FROM clips c WHERE c.game_id=g.id AND c.missing=0) AS clip_count
             FROM games g ORDER BY clip_count DESC, g.last_seen DESC",
        )?;
        let v = st.query_map([], Self::row_game)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    pub fn rename_game(&self, id: i64, name: &str) -> Result<()> {
        self.conn
            .execute("UPDATE games SET name=?2, name_locked=1 WHERE id=?1", params![id, name.trim()])?;
        Ok(())
    }

    pub fn set_game_override(&self, id: i64, is_game: Option<bool>) -> Result<()> {
        self.conn
            .execute("UPDATE games SET is_game=?2 WHERE id=?1", params![id, is_game.map(|b| b as i64)])?;
        Ok(())
    }

    /// Move everything from game `from` into game `into` and delete `from`.
    pub fn merge_games(&mut self, from: i64, into: i64) -> Result<()> {
        if from == into {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        tx.execute("UPDATE clips SET game_id=?2 WHERE game_id=?1", params![from, into])?;
        tx.execute("UPDATE activity SET game_id=?2 WHERE game_id=?1", params![from, into])?;
        tx.execute("DELETE FROM games WHERE id=?1", [from])?;
        tx.commit()?;
        Ok(())
    }

    /// exe/key -> user override, for the classifier.
    pub fn game_overrides(&self) -> Result<HashMap<String, bool>> {
        let mut st = self.conn.prepare("SELECT key, is_game FROM games WHERE is_game IS NOT NULL")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0)))?;
        Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
    }

    // -------------------------------------------------------- categories

    pub fn create_category(&self, name: &str, color: &str) -> Result<i64> {
        let name = name.trim();
        anyhow::ensure!(!name.is_empty(), "category name is empty");
        let now = crate::util::now_ms();
        let sort: i64 = self.conn.query_row("SELECT COALESCE(MAX(sort),0)+1 FROM categories", [], |r| r.get(0))?;
        self.conn.execute(
            "INSERT INTO categories(name, color, sort, created_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(name) DO NOTHING",
            params![name, color, sort, now],
        )?;
        Ok(self.conn.query_row("SELECT id FROM categories WHERE name=?1", [name], |r| r.get(0))?)
    }

    pub fn rename_category(&self, id: i64, name: &str) -> Result<()> {
        anyhow::ensure!(!name.trim().is_empty(), "category name is empty");
        self.conn
            .execute("UPDATE categories SET name=?2 WHERE id=?1", params![id, name.trim()])
            .context("a category with that name already exists")?;
        Ok(())
    }

    pub fn set_category_color(&self, id: i64, color: &str) -> Result<()> {
        self.conn.execute("UPDATE categories SET color=?2 WHERE id=?1", params![id, color])?;
        Ok(())
    }

    pub fn delete_category(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM categories WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn list_categories(&self) -> Result<Vec<Category>> {
        let mut st = self.conn.prepare(
            "SELECT c.id, c.name, c.color, c.sort,
                    (SELECT COUNT(*) FROM clip_categories cc JOIN clips k ON k.id=cc.clip_id
                     WHERE cc.category_id=c.id AND k.missing=0) AS n
             FROM categories c ORDER BY c.sort, c.name",
        )?;
        let v = st
            .query_map([], |r| {
                Ok(Category { id: r.get(0)?, name: r.get(1)?, color: r.get(2)?, sort: r.get(3)?, clip_count: r.get(4)? })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    pub fn set_clip_categories(&mut self, clip_id: i64, category_ids: &[i64]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM clip_categories WHERE clip_id=?1", [clip_id])?;
        for c in category_ids {
            tx.execute(
                "INSERT OR IGNORE INTO clip_categories(clip_id, category_id) VALUES (?1, ?2)",
                params![clip_id, c],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn add_clip_category(&self, clip_id: i64, category_id: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO clip_categories(clip_id, category_id) VALUES (?1, ?2)",
            params![clip_id, category_id],
        )?;
        Ok(())
    }

    pub fn remove_clip_category(&self, clip_id: i64, category_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM clip_categories WHERE clip_id=?1 AND category_id=?2",
            params![clip_id, category_id],
        )?;
        Ok(())
    }

    fn clip_category_ids(&self, clip_id: i64) -> Result<Vec<i64>> {
        let mut st = self.conn.prepare_cached("SELECT category_id FROM clip_categories WHERE clip_id=?1")?;
        let v = st.query_map([clip_id], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
        Ok(v)
    }

    // ------------------------------------------------------------- clips

    fn row_clip(r: &Row) -> rusqlite::Result<Clip> {
        let names: String = r.get("track_names")?;
        let mixer: String = r.get("mixer")?;
        let edits: String = r.get("audio_edits")?;
        Ok(Clip {
            id: r.get("id")?,
            path: r.get("path")?,
            title: r.get("title")?,
            game_id: r.get("game_id")?,
            game_name: r.get("game_name")?,
            created_at: r.get("created_at")?,
            source_start_ms: r.get("source_start_ms")?,
            duration: r.get("duration")?,
            size: r.get("size")?,
            width: r.get("width")?,
            height: r.get("height")?,
            fps: r.get("fps")?,
            audio_tracks: r.get("audio_tracks")?,
            track_names: serde_json::from_str(&names).unwrap_or_default(),
            favorite: r.get::<_, i64>("favorite")? != 0,
            notes: r.get("notes")?,
            source: r.get("source")?,
            mixer: parse_mix(&mixer),
            audio_edits: serde_json::from_str(&edits).unwrap_or_default(),
            thumb: r.get("thumb")?,
            probed: r.get::<_, i64>("probed")? != 0,
            missing: r.get::<_, i64>("missing")? != 0,
            last_position: r.get("last_position")?,
            views: r.get("views")?,
            category_ids: Vec::new(),
        })
    }

    const CLIP_SELECT: &'static str =
        "SELECT c.*, g.name AS game_name FROM clips c LEFT JOIN games g ON g.id = c.game_id";

    pub fn insert_clip(&self, c: &NewClip) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO clips(path, title, game_id, created_at, source_start_ms, size, source, favorite)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![c.path, c.title, c.game_id, c.created_at, c.source_start_ms, c.size, c.source, c.favorite as i64],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update_clip_probe(
        &self,
        id: i64,
        duration: f64,
        size: i64,
        width: i64,
        height: i64,
        fps: f64,
        audio_tracks: i64,
        track_names: &[String],
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE clips SET duration=?2, size=?3, width=?4, height=?5, fps=?6, audio_tracks=?7,
                              track_names=?8, probed=1 WHERE id=?1",
            params![id, duration, size, width, height, fps, audio_tracks, serde_json::to_string(track_names)?],
        )?;
        Ok(())
    }

    pub fn clip(&self, id: i64) -> Result<Option<Clip>> {
        let c = self
            .conn
            .query_row(&format!("{} WHERE c.id=?1", Self::CLIP_SELECT), [id], Self::row_clip)
            .optional()?;
        match c {
            Some(mut c) => {
                c.category_ids = self.clip_category_ids(c.id)?;
                Ok(Some(c))
            }
            None => Ok(None),
        }
    }

    pub fn clip_by_path(&self, path: &str) -> Result<Option<Clip>> {
        let id: Option<i64> = self
            .conn
            .query_row("SELECT id FROM clips WHERE path=?1", [path], |r| r.get(0))
            .optional()?;
        match id {
            Some(id) => self.clip(id),
            None => Ok(None),
        }
    }

    pub fn list_clips(&self, f: &ClipFilter) -> Result<Vec<Clip>> {
        let mut sql = format!("{} WHERE 1=1", Self::CLIP_SELECT);
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if !f.include_missing {
            sql.push_str(" AND c.missing=0");
        }
        if let Some(g) = f.game_id {
            sql.push_str(" AND c.game_id=?");
            args.push(g.into());
        }
        if let Some(cat) = f.category_id {
            sql.push_str(" AND c.id IN (SELECT clip_id FROM clip_categories WHERE category_id=?)");
            args.push(cat.into());
        }
        if f.uncategorized {
            sql.push_str(" AND c.id NOT IN (SELECT clip_id FROM clip_categories)");
        }
        if f.favorites_only {
            sql.push_str(" AND c.favorite=1");
        }
        let search = f.search.trim();
        if !search.is_empty() {
            for word in search.split_whitespace() {
                sql.push_str(
                    " AND (c.title LIKE ? ESCAPE '\\' OR c.notes LIKE ? ESCAPE '\\' OR g.name LIKE ? ESCAPE '\\'
                      OR c.id IN (SELECT cc.clip_id FROM clip_categories cc JOIN categories k ON k.id=cc.category_id
                                  WHERE k.name LIKE ? ESCAPE '\\'))",
                );
                let pat = format!("%{}%", word.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
                for _ in 0..4 {
                    args.push(pat.clone().into());
                }
            }
        }
        sql.push_str(match f.sort {
            ClipSort::Newest => " ORDER BY c.created_at DESC, c.id DESC",
            ClipSort::Oldest => " ORDER BY c.created_at ASC, c.id ASC",
            ClipSort::Longest => " ORDER BY c.duration DESC",
            ClipSort::Largest => " ORDER BY c.size DESC",
            ClipSort::Title => " ORDER BY c.title COLLATE NOCASE ASC",
            ClipSort::Game => " ORDER BY g.name COLLATE NOCASE ASC, c.created_at DESC",
        });
        let mut st = self.conn.prepare(&sql)?;
        let mut clips = st
            .query_map(rusqlite::params_from_iter(args), Self::row_clip)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // Attach categories in one query.
        let mut cats: HashMap<i64, Vec<i64>> = HashMap::new();
        let mut st2 = self.conn.prepare("SELECT clip_id, category_id FROM clip_categories")?;
        for row in st2.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
            let (c, k) = row?;
            cats.entry(c).or_default().push(k);
        }
        for c in &mut clips {
            if let Some(v) = cats.remove(&c.id) {
                c.category_ids = v;
            }
        }
        Ok(clips)
    }

    pub fn clips_between(&self, from_ms: i64, to_ms: i64) -> Result<Vec<Clip>> {
        let mut st = self.conn.prepare(&format!(
            "{} WHERE c.missing=0 AND COALESCE(c.source_start_ms, c.created_at - CAST(c.duration*1000 AS INTEGER)) < ?2
               AND c.created_at > ?1 ORDER BY c.created_at",
            Self::CLIP_SELECT
        ))?;
        let v = st.query_map(params![from_ms, to_ms], Self::row_clip)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    pub fn set_clip_path_and_title(&self, id: i64, path: &str, title: &str) -> Result<()> {
        self.conn
            .execute("UPDATE clips SET path=?2, title=?3, missing=0 WHERE id=?1", params![id, path, title])?;
        Ok(())
    }

    pub fn set_clip_game(&self, id: i64, game_id: Option<i64>) -> Result<()> {
        self.conn.execute("UPDATE clips SET game_id=?2 WHERE id=?1", params![id, game_id])?;
        Ok(())
    }

    pub fn set_clip_favorite(&self, id: i64, fav: bool) -> Result<()> {
        self.conn.execute("UPDATE clips SET favorite=?2 WHERE id=?1", params![id, fav as i64])?;
        Ok(())
    }

    pub fn set_clip_notes(&self, id: i64, notes: &str) -> Result<()> {
        self.conn.execute("UPDATE clips SET notes=?2 WHERE id=?1", params![id, notes])?;
        Ok(())
    }

    pub fn set_clip_mixer(&self, id: i64, mix: Option<&crate::mixer::MixSettings>) -> Result<()> {
        let s = match mix {
            Some(m) => serde_json::to_string(m)?,
            None => String::new(),
        };
        self.conn.execute("UPDATE clips SET mixer=?2 WHERE id=?1", params![id, s])?;
        Ok(())
    }

    pub fn set_clip_audio_edits(&self, id: i64, edits: &[AudioEdit]) -> Result<()> {
        let s = if edits.is_empty() { String::new() } else { serde_json::to_string(edits)? };
        self.conn.execute("UPDATE clips SET audio_edits=?2 WHERE id=?1", params![id, s])?;
        Ok(())
    }

    pub fn set_clip_thumb(&self, id: i64, thumb: &str) -> Result<()> {
        self.conn.execute("UPDATE clips SET thumb=?2 WHERE id=?1", params![id, thumb])?;
        Ok(())
    }

    pub fn set_clip_source_start(&self, id: i64, start_ms: Option<i64>) -> Result<()> {
        self.conn.execute("UPDATE clips SET source_start_ms=?2 WHERE id=?1", params![id, start_ms])?;
        Ok(())
    }

    pub fn set_clip_position(&self, id: i64, pos: f64) -> Result<()> {
        self.conn.execute("UPDATE clips SET last_position=?2 WHERE id=?1", params![id, pos])?;
        Ok(())
    }

    pub fn bump_clip_views(&self, id: i64) -> Result<()> {
        self.conn.execute("UPDATE clips SET views=views+1 WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn set_clip_missing(&self, id: i64, missing: bool) -> Result<()> {
        self.conn.execute("UPDATE clips SET missing=?2 WHERE id=?1", params![id, missing as i64])?;
        Ok(())
    }

    pub fn delete_clip(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM clips WHERE id=?1", [id])?;
        Ok(())
    }

    /// (id, path, missing) for every clip — used by the library scanner.
    pub fn clip_paths(&self) -> Result<Vec<(i64, String, bool)>> {
        let mut st = self.conn.prepare("SELECT id, path, missing FROM clips")?;
        let v = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    pub fn unprobed_clips(&self, limit: usize) -> Result<Vec<(i64, String)>> {
        let mut st = self
            .conn
            .prepare("SELECT id, path FROM clips WHERE (probed=0 OR thumb='') AND missing=0 ORDER BY created_at DESC LIMIT ?1")?;
        let v = st
            .query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    pub fn clip_stats(&self) -> Result<(i64, i64)> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(size),0) FROM clips WHERE missing=0",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    // -------------------------------------------------------- recordings

    fn row_recording(r: &Row) -> rusqlite::Result<Recording> {
        Ok(Recording {
            id: r.get("id")?,
            path: r.get("path")?,
            start_ms: r.get("start_ms")?,
            duration: r.get("duration")?,
            size: r.get("size")?,
            status: r.get("status")?,
            audio_tracks: r.get("audio_tracks")?,
            kept: r.get::<_, i64>("kept")? != 0,
            probed: r.get::<_, i64>("probed")? != 0,
        })
    }

    pub fn upsert_recording(&self, path: &str, start_ms: i64, size: i64, status: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO recordings(path, start_ms, size, status) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET size=excluded.size, status=excluded.status",
            params![path, start_ms, size, status],
        )?;
        Ok(self.conn.query_row("SELECT id FROM recordings WHERE path=?1", [path], |r| r.get(0))?)
    }

    pub fn set_recording_probe(&self, id: i64, duration: f64, audio_tracks: i64, final_probe: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE recordings SET duration=?2, audio_tracks=?3, probed=?4 WHERE id=?1",
            params![id, duration, audio_tracks, final_probe as i64],
        )?;
        Ok(())
    }

    pub fn set_recording_duration(&self, id: i64, duration: f64) -> Result<()> {
        self.conn.execute("UPDATE recordings SET duration=?2 WHERE id=?1", params![id, duration])?;
        Ok(())
    }

    pub fn set_recording_kept(&self, id: i64, kept: bool, new_path: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE recordings SET kept=?2, path=?3 WHERE id=?1",
            params![id, kept as i64, new_path],
        )?;
        Ok(())
    }

    pub fn delete_recording_by_path(&self, path: &str) -> Result<()> {
        self.conn.execute("DELETE FROM recordings WHERE path=?1", [path])?;
        Ok(())
    }

    pub fn recording(&self, id: i64) -> Result<Option<Recording>> {
        Ok(self
            .conn
            .query_row("SELECT * FROM recordings WHERE id=?1", [id], Self::row_recording)
            .optional()?)
    }

    pub fn list_recordings(&self) -> Result<Vec<Recording>> {
        let mut st = self.conn.prepare("SELECT * FROM recordings ORDER BY start_ms, path")?;
        let v = st.query_map([], Self::row_recording)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    // ---------------------------------------------------------- activity

    pub fn start_activity(&self, key: &str, game_id: Option<i64>, is_game: bool, title: &str, at: i64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO activity(key, game_id, is_game, title, start_ms, end_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
            params![key, game_id, is_game as i64, title, at],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn extend_activity(&self, id: i64, end_ms: i64, title: Option<&str>) -> Result<()> {
        match title {
            Some(t) => self
                .conn
                .execute("UPDATE activity SET end_ms=?2, title=?3 WHERE id=?1", params![id, end_ms, t])?,
            None => self.conn.execute("UPDATE activity SET end_ms=?2 WHERE id=?1", params![id, end_ms])?,
        };
        Ok(())
    }

    pub fn activity_between(&self, from_ms: i64, to_ms: i64) -> Result<Vec<ActivitySpan>> {
        let mut st = self.conn.prepare(
            "SELECT a.id, a.key, a.game_id, COALESCE(g.name, a.key), a.is_game, a.title, a.start_ms, a.end_ms,
                    g.is_game
             FROM activity a LEFT JOIN games g ON g.id=a.game_id
             WHERE a.end_ms > ?1 AND a.start_ms < ?2 ORDER BY a.start_ms",
        )?;
        let v = st
            .query_map(params![from_ms, to_ms], |r| {
                let ov: Option<i64> = r.get(8)?;
                Ok(ActivitySpan {
                    id: r.get(0)?,
                    key: r.get(1)?,
                    game_id: r.get(2)?,
                    name: r.get(3)?,
                    is_game: ov.map(|v| v != 0).unwrap_or(r.get::<_, i64>(4)? != 0),
                    title: r.get(5)?,
                    start_ms: r.get(6)?,
                    end_ms: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    pub fn prune_activity(&self, before_ms: i64) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM activity WHERE end_ms < ?1", [before_ms])?)
    }

    // ----------------------------------------------------------- markers

    pub fn add_marker(&self, at_ms: i64, label: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO markers(at_ms, label, created_at) VALUES (?1, ?2, ?3)",
            params![at_ms, label, crate::util::now_ms()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn markers_between(&self, from_ms: i64, to_ms: i64) -> Result<Vec<Marker>> {
        let mut st = self
            .conn
            .prepare("SELECT id, at_ms, label, created_at FROM markers WHERE at_ms >= ?1 AND at_ms <= ?2 ORDER BY at_ms")?;
        let v = st
            .query_map(params![from_ms, to_ms], |r| {
                Ok(Marker { id: r.get(0)?, at_ms: r.get(1)?, label: r.get(2)?, created_at: r.get(3)? })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    pub fn rename_marker(&self, id: i64, label: &str) -> Result<()> {
        self.conn.execute("UPDATE markers SET label=?2 WHERE id=?1", params![id, label])?;
        Ok(())
    }

    pub fn delete_marker(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM markers WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn prune_markers(&self, before_ms: i64) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM markers WHERE at_ms < ?1", [before_ms])?)
    }

    // ------------------------------------------------------------ events

    pub fn log_event(&self, level: &str, message: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO events(at_ms, level, message) VALUES (?1, ?2, ?3)",
            params![crate::util::now_ms(), level, message],
        )?;
        Ok(())
    }

    pub fn recent_events(&self, limit: usize) -> Result<Vec<EventEntry>> {
        let mut st = self
            .conn
            .prepare("SELECT id, at_ms, level, message FROM events ORDER BY id DESC LIMIT ?1")?;
        let v = st
            .query_map([limit as i64], |r| {
                Ok(EventEntry { id: r.get(0)?, at_ms: r.get(1)?, level: r.get(2)?, message: r.get(3)? })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    pub fn prune_events(&self, keep: usize) -> Result<()> {
        self.conn.execute(
            "DELETE FROM events WHERE id NOT IN (SELECT id FROM events ORDER BY id DESC LIMIT ?1)",
            [keep as i64],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_clip(db: &Db, path: &str, game: Option<i64>, at: i64) -> i64 {
        db.insert_clip(&NewClip {
            path: path.into(),
            title: path.trim_end_matches(".mp4").into(),
            game_id: game,
            created_at: at,
            source_start_ms: None,
            size: 100,
            source: "replay".into(),
            favorite: false,
        })
        .unwrap()
    }

    #[test]
    fn schema_is_idempotent_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("lib.db");
        {
            let db = Db::open(&p).unwrap();
            db.meta_set("x", "1").unwrap();
        }
        let db = Db::open(&p).unwrap();
        assert_eq!(db.meta_get("x").unwrap().as_deref(), Some("1"));
        assert_eq!(db.meta_get("schema_version").unwrap().as_deref(), Some("2"));
    }

    #[test]
    fn games_upsert_respects_user_rename() {
        let db = Db::open_in_memory().unwrap();
        let id = db.upsert_game("deadlock.exe", "Deadlock", r"C:\x\deadlock.exe", true, 10).unwrap();
        let id2 = db.upsert_game("deadlock.exe", "Project8", "", false, 20).unwrap();
        assert_eq!(id, id2);
        let g = db.game(id).unwrap().unwrap();
        assert_eq!(g.name, "Project8");
        assert_eq!(g.exe_path, r"C:\x\deadlock.exe");
        assert!(g.auto_is_game, "auto flag never downgrades");
        assert_eq!(g.last_seen, 20);
        db.rename_game(id, "Deadlock (beta)").unwrap();
        db.upsert_game("deadlock.exe", "Something else", "", true, 30).unwrap();
        assert_eq!(db.game(id).unwrap().unwrap().name, "Deadlock (beta)");
        db.set_game_override(id, Some(false)).unwrap();
        assert_eq!(db.game_overrides().unwrap().get("deadlock.exe"), Some(&false));
        assert!(!db.game(id).unwrap().unwrap().is_game());
        assert_eq!(db.game_by_name("deadlock (BETA)").unwrap().unwrap().id, id);
    }

    #[test]
    fn clips_filters_and_categories() {
        let mut db = Db::open_in_memory().unwrap();
        let g1 = db.upsert_game("a.exe", "Alpha", "", true, 0).unwrap();
        let g2 = db.upsert_game("b.exe", "Beta", "", true, 0).unwrap();
        let c1 = new_clip(&db, "one.mp4", Some(g1), 1000);
        let c2 = new_clip(&db, "two.mp4", Some(g2), 2000);
        let c3 = new_clip(&db, "three_100%.mp4", None, 3000);
        let funny = db.create_category("Funny", "#fff").unwrap();
        let best = db.create_category("Best", "").unwrap();
        assert_eq!(db.create_category("funny", "").unwrap(), funny, "case-insensitive unique");
        db.set_clip_categories(c1, &[funny, best]).unwrap();
        db.add_clip_category(c2, funny).unwrap();
        db.set_clip_favorite(c2, true).unwrap();

        let all = db.list_clips(&ClipFilter::default()).unwrap();
        assert_eq!(all.iter().map(|c| c.id).collect::<Vec<_>>(), vec![c3, c2, c1]);
        assert_eq!(all[2].category_ids.len(), 2);
        assert_eq!(all[2].game_name.as_deref(), Some("Alpha"));

        let f = |filter: ClipFilter| db.list_clips(&filter).unwrap().iter().map(|c| c.id).collect::<Vec<_>>();
        assert_eq!(f(ClipFilter { game_id: Some(g1), ..Default::default() }), vec![c1]);
        assert_eq!(f(ClipFilter { category_id: Some(funny), ..Default::default() }), vec![c2, c1]);
        assert_eq!(f(ClipFilter { favorites_only: true, ..Default::default() }), vec![c2]);
        assert_eq!(f(ClipFilter { uncategorized: true, ..Default::default() }), vec![c3]);
        assert_eq!(f(ClipFilter { search: "beta".into(), ..Default::default() }), vec![c2]);
        assert_eq!(f(ClipFilter { search: "funny one".into(), ..Default::default() }), vec![c1]);
        assert_eq!(f(ClipFilter { search: "100%".into(), ..Default::default() }), vec![c3]);
        assert_eq!(f(ClipFilter { search: "_".into(), ..Default::default() }), vec![c3]);
        assert_eq!(f(ClipFilter { sort: ClipSort::Oldest, ..Default::default() }), vec![c1, c2, c3]);

        let cats = db.list_categories().unwrap();
        let funny_cat = cats.iter().find(|c| c.id == funny).unwrap();
        assert_eq!(funny_cat.clip_count, 2);

        db.set_clip_missing(c1, true).unwrap();
        assert_eq!(f(ClipFilter::default()), vec![c3, c2]);
        db.delete_category(funny).unwrap();
        assert_eq!(db.clip(c2).unwrap().unwrap().category_ids, Vec::<i64>::new());
        db.delete_clip(c2).unwrap();
        assert!(db.clip(c2).unwrap().is_none());
    }

    #[test]
    fn clip_probe_mixer_and_merge() {
        let mut db = Db::open_in_memory().unwrap();
        let g1 = db.upsert_game("folder:arc raiders", "ARC Raiders", "", true, 0).unwrap();
        let g2 = db.upsert_game("pioneergame.exe", "ARC Raiders", "", true, 0).unwrap();
        let c = new_clip(&db, "x.mp4", Some(g1), 5);
        db.update_clip_probe(c, 12.5, 999, 2560, 1440, 60.0, 6, &["Full mix".into(), "Game".into()]).unwrap();
        let mut m = crate::mixer::MixSettings::default();
        m.set("cat:voice", crate::mixer::LaneGain { gain: 0.5, muted: false });
        db.set_clip_mixer(c, Some(&m)).unwrap();
        let clip = db.clip(c).unwrap().unwrap();
        assert!(clip.probed);
        assert_eq!(clip.track_names, vec!["Full mix", "Game"]);
        assert_eq!(clip.mixer, Some(m));
        // Old per-track gain lists still load.
        db.conn().execute("UPDATE clips SET mixer='[0.0,1.0,0.5,1.0,1.0,0.0]' WHERE id=?1", [c]).unwrap();
        let old = db.clip(c).unwrap().unwrap().mixer.unwrap();
        assert_eq!(old.lane("cat:voice").gain, 0.5);
        db.set_clip_audio_edits(c, &[AudioEdit::new("cat:voice", 1.0, 2.5, 0.0)]).unwrap();
        assert_eq!(db.clip(c).unwrap().unwrap().audio_edits, vec![AudioEdit::new("cat:voice", 1.0, 2.5, 0.0)]);
        db.set_clip_audio_edits(c, &[]).unwrap();
        assert!(db.clip(c).unwrap().unwrap().audio_edits.is_empty());
        db.set_clip_mixer(c, None).unwrap();
        assert_eq!(db.clip(c).unwrap().unwrap().mixer, None);
        db.merge_games(g1, g2).unwrap();
        assert_eq!(db.clip(c).unwrap().unwrap().game_id, Some(g2));
        assert!(db.game(g1).unwrap().is_none());
        assert_eq!(db.clip_by_path("X.MP4").unwrap().unwrap().id, c, "paths are case-insensitive");
    }

    #[test]
    fn recordings_activity_markers_events() {
        let db = Db::open_in_memory().unwrap();
        let r = db.upsert_recording(r"I:\a.mp4", 1000, 10, "recording").unwrap();
        let r2 = db.upsert_recording(r"I:\a.mp4", 1000, 20, "complete").unwrap();
        assert_eq!(r, r2);
        db.set_recording_probe(r, 3600.0, 6, true).unwrap();
        let rec = db.recording(r).unwrap().unwrap();
        assert_eq!((rec.size, rec.status.as_str(), rec.audio_tracks), (20, "complete", 6));
        assert_eq!(rec.end_ms(), 1000 + 3_600_000);
        db.set_recording_kept(r, true, r"I:\Kept\a.mp4").unwrap();
        assert!(db.list_recordings().unwrap()[0].kept);
        db.delete_recording_by_path(r"I:\Kept\a.mp4").unwrap();
        assert!(db.list_recordings().unwrap().is_empty());

        let g = db.upsert_game("cs2.exe", "Counter-Strike 2", "", true, 0).unwrap();
        let a = db.start_activity("cs2.exe", Some(g), true, "CS2", 100).unwrap();
        db.extend_activity(a, 500, None).unwrap();
        let spans = db.activity_between(0, 1000).unwrap();
        assert_eq!(spans.len(), 1);
        assert_eq!((spans[0].name.as_str(), spans[0].end_ms), ("Counter-Strike 2", 500));
        assert!(db.activity_between(600, 1000).unwrap().is_empty());
        assert_eq!(db.prune_activity(1000).unwrap(), 1);

        let m = db.add_marker(700, "nice").unwrap();
        db.rename_marker(m, "great").unwrap();
        assert_eq!(db.markers_between(0, 1000).unwrap()[0].label, "great");
        db.delete_marker(m).unwrap();
        assert!(db.markers_between(0, 1000).unwrap().is_empty());

        for i in 0..10 {
            db.log_event("info", &format!("e{i}")).unwrap();
        }
        db.prune_events(3).unwrap();
        let ev = db.recent_events(100).unwrap();
        assert_eq!(ev.iter().map(|e| e.message.as_str()).collect::<Vec<_>>(), vec!["e9", "e8", "e7"]);
    }

    #[test]
    fn concurrent_connections_see_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("lib.db");
        let a = Db::open(&p).unwrap();
        let b = Db::open(&p).unwrap();
        let id = a.create_category("Shared", "").unwrap();
        assert_eq!(b.list_categories().unwrap()[0].id, id);
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    /// A v1 database (no audio_edits column) upgrades in place.
    #[test]
    fn upgrades_v1_database() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("old.db");
        {
            let c = Connection::open(&p).unwrap();
            c.execute_batch(SCHEMA_V1).unwrap();
            c.execute("INSERT INTO meta(key, value) VALUES ('schema_version', '1')", []).unwrap();
            c.execute("INSERT INTO clips(path, title, created_at) VALUES ('a.mp4', 'a', 1)", []).unwrap();
        }
        let db = Db::open(&p).unwrap();
        let clip = db.clip_by_path("a.mp4").unwrap().unwrap();
        assert!(clip.audio_edits.is_empty());
        assert_eq!(db.meta_get("schema_version").unwrap().as_deref(), Some("2"));
    }
}

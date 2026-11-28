//! Service <-> GUI protocol: newline-delimited JSON over a loopback TCP socket.
//!
//! The service writes `ipc.json` (port + random token) into the data dir; the
//! GUI reads it, connects to 127.0.0.1 and authenticates with the token before
//! anything else is accepted.

use crate::audio_names::Endpoint;
use anyhow::{bail, Context, Result};
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IpcInfo {
    pub port: u16,
    pub token: String,
    pub pid: u32,
}

impl IpcInfo {
    pub fn read() -> Result<IpcInfo> {
        let text = std::fs::read_to_string(crate::paths::ipc_info_path()).context("service is not running")?;
        Ok(serde_json::from_str(&text)?)
    }
    pub fn write(&self) -> Result<()> {
        crate::util::write_atomic(&crate::paths::ipc_info_path(), serde_json::to_string(self)?.as_bytes())?;
        Ok(())
    }
}

/// Where an export takes its picture and sound from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum ExportSource {
    Clip { clip_id: i64 },
    /// A wall-clock range of the 24/7 recording.
    Recording { from_ms: i64, to_ms: i64 },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Auth { token: String },
    Status,
    /// Save the last `clip_length_secs` (or `short_clip_secs`) as a clip.
    SaveClip { short: bool },
    AddMarker { label: String },
    /// Pause/resume the 24/7 recording.
    SetRecording { enabled: bool },
    #[serde(alias = "restart_obs")]
    RestartRecorder,
    /// Config file changed; reload it (restarting the recorder if needed).
    ReloadConfig {
        #[serde(alias = "restart_obs", default)]
        restart_recorder: bool,
    },
    /// Make a clip from a wall-clock range of the continuous recording.
    /// `mix` and `audio_edits` (wall-clock seconds) are remembered with the clip.
    CutRange {
        from_ms: i64,
        to_ms: i64,
        title: String,
        game_id: Option<i64>,
        category_ids: Vec<i64>,
        #[serde(default)]
        mix: Option<crate::mixer::MixSettings>,
        #[serde(default)]
        audio_edits: Vec<crate::model::AudioEdit>,
    },
    /// Trim an existing clip into a new clip (or replace it).
    TrimClip { clip_id: i64, start_s: f64, end_s: f64, replace: bool },
    /// Export a shareable copy with one mixed audio track. `range` is in the
    /// source's seconds; `edits` in the same time base (clip seconds, or
    /// wall-clock seconds for a recording range). Omitted edits use stored clip edits.
    Export {
        source: ExportSource,
        #[serde(default)]
        range: Option<(f64, f64)>,
        mix: crate::mixer::MixSettings,
        #[serde(default)]
        edits: Option<Vec<crate::model::AudioEdit>>,
        target_size_mb: Option<u32>,
        #[serde(default)]
        title: String,
    },
    CancelJob { id: u64 },
    AudioDevices,
    Monitors,
    RescanLibrary,
    KeepRecording { recording_id: i64, keep: bool },
    /// Delete clips (Recycle Bin by default).
    DeleteClips { clip_ids: Vec<i64> },
    /// Move a clip into another game's folder.
    SetClipGame { clip_id: i64, game_id: Option<i64> },
    RenameClip { clip_id: i64, title: String },
    /// Copy a file to the clipboard (so it can be pasted into Discord etc).
    CopyFileToClipboard { path: String },
    /// Temporarily release global hotkeys (while the GUI records a new one).
    SuspendHotkeys { suspend: bool },
    /// Mark that the GUI changed library metadata (rename/categories) so other
    /// windows refresh.
    Touch,
    Shutdown,
}

/// The recording engine (screen + sound capture, encoders).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct EngineStatus {
    pub running: bool,
    pub encoder: String,
    pub capture: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub dropped_frames: u64,
    pub last_error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RecStatus {
    pub active: bool,
    pub paused_by_user: bool,
    pub path: String,
    pub started_ms: i64,
    #[serde(default)]
    pub written_until_ms: i64,
    pub bytes: i64,
    pub healthy: bool,
    pub tracks: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ReplayStatus {
    /// The clip hotkey works right now.
    pub active: bool,
    pub seconds: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct MicStatus {
    /// "ok", "fallback", "missing", "disabled", "unknown".
    pub state: String,
    pub device: String,
    pub preferred: String,
    pub muted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct GameStatus {
    pub current: Option<String>,
    pub audio_target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct DiskStatus {
    pub free_bytes: u64,
    pub total_bytes: u64,
    pub recordings_bytes: u64,
    pub recordings_count: u32,
    pub oldest_ms: Option<i64>,
    pub clips_bytes: u64,
    pub clips_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct JobInfo {
    pub id: u64,
    pub kind: String,
    pub title: String,
    pub progress: f32,
    /// "running", "done", "failed", "cancelled".
    pub state: String,
    pub message: String,
    pub output: Option<String>,
    pub clip_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Status {
    pub version: String,
    pub service_started_ms: i64,
    #[serde(default)]
    pub engine: EngineStatus,
    pub recording: RecStatus,
    pub replay: ReplayStatus,
    pub mic: MicStatus,
    pub game: GameStatus,
    pub disk: DiskStatus,
    pub warnings: Vec<String>,
    pub jobs: Vec<JobInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Meter {
    pub name: String,
    /// Peak level 0..1 (linear).
    pub peak: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AudioDevices {
    pub inputs: Vec<Endpoint>,
    pub outputs: Vec<Endpoint>,
    pub default_input: Option<String>,
    pub default_output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub device_name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Status { status: Box<Status> },
    ClipAdded { clip_id: i64 },
    LibraryChanged,
    RecordingsChanged,
    Job { job: JobInfo },
    Toast { level: String, message: String },
    Meters { meters: Vec<Meter> },
}

/// Server -> client message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Response { id: u64, ok: bool, #[serde(default)] data: Value, #[serde(default)] error: Option<String> },
    Event { event: Event },
}

/// Client -> server message.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientMsg {
    pub id: u64,
    pub req: Request,
}

pub fn write_line<T: Serialize>(w: &mut impl Write, msg: &T) -> std::io::Result<()> {
    let mut s = serde_json::to_string(msg).map_err(std::io::Error::other)?;
    s.push('\n');
    w.write_all(s.as_bytes())?;
    w.flush()
}

type PendingMap = Arc<Mutex<HashMap<u64, Sender<Result<Value, String>>>>>;

/// GUI-side connection to the service.
pub struct IpcClient {
    writer: Mutex<TcpStream>,
    pending: PendingMap,
    alive: Arc<AtomicBool>,
    next: AtomicU64,
}

impl IpcClient {
    pub fn connect(timeout: Duration) -> Result<(IpcClient, Receiver<Event>)> {
        let info = IpcInfo::read()?;
        Self::connect_to(info.port, &info.token, timeout)
    }

    pub fn connect_to(port: u16, token: &str, timeout: Duration) -> Result<(IpcClient, Receiver<Event>)> {
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        let stream = TcpStream::connect_timeout(&addr, timeout).context("service is not running")?;
        stream.set_nodelay(true).ok();
        let reader = stream.try_clone()?;
        let (ev_tx, ev_rx) = unbounded();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        {
            let pending = pending.clone();
            let alive = alive.clone();
            std::thread::Builder::new()
                .name("ipc-client".into())
                .spawn(move || {
                    let r = BufReader::new(reader);
                    for line in r.lines() {
                        let Ok(line) = line else { break };
                        match serde_json::from_str::<ServerMsg>(&line) {
                            Ok(ServerMsg::Response { id, ok, data, error }) => {
                                if let Some(tx) = pending.lock().remove(&id) {
                                    let _ = tx.send(if ok { Ok(data) } else { Err(error.unwrap_or_default()) });
                                }
                            }
                            Ok(ServerMsg::Event { event }) => {
                                let _ = ev_tx.send(event);
                            }
                            Err(e) => tracing::warn!("bad ipc message: {e}"),
                        }
                    }
                    alive.store(false, Ordering::SeqCst);
                    for (_, tx) in pending.lock().drain() {
                        let _ = tx.send(Err("disconnected from service".into()));
                    }
                })?;
        }
        let client = IpcClient { writer: Mutex::new(stream), pending, alive, next: AtomicU64::new(1) };
        client.call(Request::Auth { token: token.to_string() }, timeout)?;
        Ok((client, ev_rx))
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn call(&self, req: Request, timeout: Duration) -> Result<Value> {
        if !self.is_alive() {
            bail!("disconnected from service");
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = bounded(1);
        self.pending.lock().insert(id, tx);
        {
            let mut w = self.writer.lock();
            if let Err(e) = write_line(&mut *w, &ClientMsg { id, req }) {
                self.pending.lock().remove(&id);
                self.alive.store(false, Ordering::SeqCst);
                bail!("send failed: {e}");
            }
        }
        match rx.recv_timeout(timeout) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => bail!("{e}"),
            Err(_) => {
                self.pending.lock().remove(&id);
                bail!("service did not respond")
            }
        }
    }

    /// Fire-and-forget: send and return immediately; errors arrive as toasts.
    pub fn send(&self, req: Request) -> Result<()> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let mut w = self.writer.lock();
        write_line(&mut *w, &ClientMsg { id, req })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn wire_format_is_stable() {
        let m = ClientMsg { id: 3, req: Request::SaveClip { short: true } };
        assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"id":3,"req":{"cmd":"save_clip","short":true}}"#);
        let ev = ServerMsg::Event { event: Event::Toast { level: "info".into(), message: "hi".into() } };
        let s = serde_json::to_string(&ev).unwrap();
        assert_eq!(s, r#"{"type":"event","event":{"kind":"toast","level":"info","message":"hi"}}"#);
        let back: ServerMsg = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ev);
        let st = ServerMsg::Event { event: Event::Status { status: Box::default() } };
        let back: ServerMsg = serde_json::from_str(&serde_json::to_string(&st).unwrap()).unwrap();
        assert_eq!(back, st);
    }

    #[test]
    fn client_roundtrip_with_fake_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut w = s.try_clone().unwrap();
            let r = BufReader::new(s);
            for line in r.lines() {
                let msg: ClientMsg = serde_json::from_str(&line.unwrap()).unwrap();
                let reply = match msg.req {
                    Request::Auth { token } if token == "tok" => ServerMsg::Response { id: msg.id, ok: true, data: Value::Null, error: None },
                    Request::Auth { .. } => ServerMsg::Response { id: msg.id, ok: false, data: Value::Null, error: Some("bad token".into()) },
                    Request::Status => {
                        write_line(&mut w, &ServerMsg::Event { event: Event::LibraryChanged }).unwrap();
                        ServerMsg::Response { id: msg.id, ok: true, data: serde_json::json!({"x": 1}), error: None }
                    }
                    _ => ServerMsg::Response { id: msg.id, ok: false, data: Value::Null, error: Some("unsupported".into()) },
                };
                write_line(&mut w, &reply).unwrap();
            }
        });
        let (c, events) = IpcClient::connect_to(port, "tok", Duration::from_secs(2)).unwrap();
        let v = c.call(Request::Status, Duration::from_secs(2)).unwrap();
        assert_eq!(v["x"], 1);
        assert_eq!(events.recv_timeout(Duration::from_secs(2)).unwrap(), Event::LibraryChanged);
        let e = c.call(Request::RestartRecorder, Duration::from_secs(2)).unwrap_err();
        assert!(e.to_string().contains("unsupported"));
    }

    #[test]
    fn bad_token_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let mut w = s.try_clone().unwrap();
            let mut line = String::new();
            BufReader::new(s).read_line(&mut line).unwrap();
            let msg: ClientMsg = serde_json::from_str(&line).unwrap();
            write_line(&mut w, &ServerMsg::Response { id: msg.id, ok: false, data: Value::Null, error: Some("bad token".into()) }).unwrap();
        });
        assert!(IpcClient::connect_to(port, "nope", Duration::from_secs(2)).is_err());
    }
}

//! Loopback TCP server the GUI connects to.

use crate::jobs::{self, JobKind};
use crate::shared::{LibCmd, Shared, SupCmd};
use crate::supervisor::send_and_wait;
use anyhow::Result;
use cv_core::ipc::{ClientMsg, Event, IpcInfo, Request, ServerMsg};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

pub fn start(sh: Arc<Shared>) -> Result<IpcInfo> {
    let base = sh.cfg.read().advanced.ipc_port;
    let mut listener = None;
    for port in base..base.saturating_add(20) {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", port)) {
            listener = Some((l, port));
            break;
        }
    }
    let (listener, port) = listener.ok_or_else(|| anyhow::anyhow!("no free port for the GUI connection"))?;
    let info = IpcInfo { port, token: cv_core::util::random_token(24), pid: std::process::id() };
    info.write()?;
    tracing::info!("GUI connection listening on 127.0.0.1:{port}");
    let token = info.token.clone();
    std::thread::Builder::new().name("ipc-accept".into()).spawn(move || {
        for stream in listener.incoming() {
            if sh.is_shutting_down() {
                break;
            }
            let Ok(stream) = stream else { continue };
            let sh = sh.clone();
            let token = token.clone();
            let _ = std::thread::Builder::new().name("ipc-conn".into()).spawn(move || {
                if let Err(e) = connection(sh, stream, &token) {
                    tracing::debug!("gui connection ended: {e:#}");
                }
            });
        }
    })?;
    Ok(info)
}

fn connection(sh: Arc<Shared>, stream: TcpStream, token: &str) -> Result<()> {
    stream.set_nodelay(true).ok();
    let mut writer = stream.try_clone()?;
    let (out_tx, out_rx) = crossbeam_channel::unbounded::<String>();
    std::thread::spawn(move || {
        use std::io::Write;
        for line in out_rx {
            if writer.write_all(line.as_bytes()).and_then(|_| writer.write_all(b"\n")).and_then(|_| writer.flush()).is_err() {
                break;
            }
        }
    });
    let reply = |id: u64, r: Result<Value, String>| {
        let msg = match r {
            Ok(data) => ServerMsg::Response { id, ok: true, data, error: None },
            Err(e) => ServerMsg::Response { id, ok: false, data: Value::Null, error: Some(e) },
        };
        if let Ok(s) = serde_json::to_string(&msg) {
            let _ = out_tx.send(s);
        }
    };
    let mut authed = false;
    let reader = BufReader::new(stream.try_clone()?);
    for line in reader.lines() {
        let line = line?;
        let msg: ClientMsg = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("bad request from GUI: {e}");
                continue;
            }
        };
        if !authed {
            match msg.req {
                Request::Auth { token: t } if t == token => {
                    authed = true;
                    reply(msg.id, Ok(Value::Null));
                    sh.clients.lock().push(out_tx.clone());
                    let status = Box::new(crate::status::build(&sh));
                    if let Ok(s) = serde_json::to_string(&ServerMsg::Event { event: Event::Status { status } }) {
                        let _ = out_tx.send(s);
                    }
                    continue;
                }
                _ => {
                    reply(msg.id, Err("not authorised".into()));
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    return Ok(());
                }
            }
        }
        let r = handle(&sh, msg.req);
        reply(msg.id, r);
    }
    Ok(())
}

fn handle(sh: &Arc<Shared>, req: Request) -> Result<Value, String> {
    let e2s = |e: anyhow::Error| format!("{e:#}");
    match req {
        Request::Auth { .. } => Ok(Value::Null),
        Request::Status => Ok(serde_json::to_value(crate::status::build(sh)).unwrap_or_default()),
        Request::SaveClip { short } => {
            send_and_wait(&sh.sup_tx, |r| SupCmd::SaveClip { short, reply: Some(r) }, Duration::from_secs(15)).map(|_| Value::Null)
        }
        Request::AddMarker { label } => crate::add_marker(sh, &label).map(|id| json!({"id": id})).map_err(e2s),
        Request::SetRecording { enabled } => {
            send_and_wait(&sh.sup_tx, |r| SupCmd::SetRecording { enabled, reply: Some(r) }, Duration::from_secs(20)).map(|_| Value::Null)
        }
        Request::RestartObs => {
            send_and_wait(&sh.sup_tx, |r| SupCmd::RestartObs { reply: Some(r) }, Duration::from_secs(90)).map(|_| Value::Null)
        }
        Request::ReloadConfig { restart_obs } => {
            crate::reload_config(sh, restart_obs).map_err(e2s)?;
            Ok(Value::Null)
        }
        Request::CutRange { from_ms, to_ms, title, game_id, category_ids, precise, gains, audio_edits } => {
            let id = jobs::submit(sh, JobKind::CutRange { from_ms, to_ms, title, game_id, category_ids, precise, gains, audio_edits });
            Ok(json!({"job_id": id}))
        }
        Request::TrimClip { clip_id, start_s, end_s, replace, precise } => {
            let id = jobs::submit(sh, JobKind::Trim { clip_id, start_s, end_s, replace, precise });
            Ok(json!({"job_id": id}))
        }
        Request::Export { clip_id, range, gains, target_size_mb } => {
            let id = jobs::submit(sh, JobKind::Export { clip_id, range, gains, target_size_mb });
            Ok(json!({"job_id": id}))
        }
        Request::CancelJob { id } => Ok(json!({"cancelled": jobs::cancel(id)})),
        Request::AudioDevices => {
            let _ = sh.audio_tx.try_send(());
            Ok(serde_json::to_value(sh.devices.lock().clone()).unwrap_or_default())
        }
        Request::RescanLibrary => {
            let _ = sh.lib_tx.send(LibCmd::Rescan);
            Ok(Value::Null)
        }
        Request::KeepRecording { recording_id, keep } => {
            crate::library::set_kept(sh, recording_id, keep).map(|_| Value::Null).map_err(e2s)
        }
        Request::DeleteClips { clip_ids } => {
            let n = crate::clips::delete_clips(sh, &clip_ids).map_err(e2s)?;
            sh.emit(Event::LibraryChanged);
            Ok(json!({"deleted": n}))
        }
        Request::SetClipGame { clip_id, game_id } => {
            crate::clips::set_clip_game(sh, clip_id, game_id).map_err(e2s)?;
            sh.emit(Event::LibraryChanged);
            Ok(Value::Null)
        }
        Request::RenameClip { clip_id, title } => {
            crate::clips::rename_clip(sh, clip_id, &title).map_err(e2s)?;
            sh.emit(Event::LibraryChanged);
            Ok(Value::Null)
        }
        Request::CopyFileToClipboard { path } => {
            crate::win::shell::copy_file_to_clipboard(std::path::Path::new(&path)).map_err(e2s)?;
            Ok(Value::Null)
        }
        Request::SuspendHotkeys { suspend } => {
            sh.ui.send(crate::win::ui::UiCmd::SuspendHotkeys(suspend));
            Ok(Value::Null)
        }
        Request::Touch => {
            sh.emit(Event::LibraryChanged);
            Ok(Value::Null)
        }
        Request::Shutdown => {
            crate::request_shutdown(sh);
            Ok(Value::Null)
        }
    }
}

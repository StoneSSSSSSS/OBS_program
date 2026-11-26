//! Blocking obs-websocket v5 client.
//!
//! One background thread owns the socket. Requests are sent through a channel
//! and matched to responses by id; events are delivered on a channel returned
//! from `connect`. When the connection drops, all pending requests fail fast
//! with `ObsError::Disconnected` and a final `ObsEvent::Disconnected` is sent.

use super::protocol::*;
use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tungstenite::client::IntoClientRequest;
use tungstenite::Message;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ObsError {
    #[error("not connected to OBS")]
    Disconnected,
    #[error("OBS did not answer {0} in time")]
    Timeout(String),
    #[error("OBS rejected {request}: {comment} (code {code})")]
    Request { request: String, code: i64, comment: String },
    #[error("could not connect to OBS: {0}")]
    Connect(String),
    #[error("OBS websocket authentication failed (wrong password?)")]
    Auth,
}

#[derive(Debug, Clone)]
pub enum ObsEvent {
    Event { kind: String, data: Value },
    Disconnected(String),
}

type Pending = Arc<Mutex<HashMap<String, (String, Sender<Result<Value, ObsError>>)>>>;

pub struct ObsClient {
    out_tx: Sender<String>,
    pending: Pending,
    alive: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    next_id: AtomicU64,
    pub obs_version: String,
    pub ws_version: String,
    pub request_timeout: Duration,
}

impl ObsClient {
    pub fn connect(
        port: u16,
        password: &str,
        event_mask: u32,
        timeout: Duration,
    ) -> Result<(ObsClient, Receiver<ObsEvent>), ObsError> {
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        let stream =
            TcpStream::connect_timeout(&addr, timeout).map_err(|e| ObsError::Connect(e.to_string()))?;
        stream.set_nodelay(true).ok();
        stream.set_read_timeout(Some(timeout)).ok();
        stream.set_write_timeout(Some(timeout)).ok();

        let mut req = format!("ws://127.0.0.1:{port}")
            .into_client_request()
            .map_err(|e| ObsError::Connect(e.to_string()))?;
        req.headers_mut()
            .insert("Sec-WebSocket-Protocol", "obswebsocket.json".parse().unwrap());
        let (mut ws, _resp) =
            tungstenite::client(req, stream).map_err(|e| ObsError::Connect(e.to_string()))?;

        // Hello
        let hello = read_json(&mut ws)?;
        if hello["op"].as_u64() != Some(OP_HELLO) {
            return Err(ObsError::Connect("expected Hello".into()));
        }
        let d = &hello["d"];
        let obs_ws_version = d["obsWebSocketVersion"].as_str().unwrap_or("").to_string();
        let mut identify = json!({"rpcVersion": 1, "eventSubscriptions": event_mask});
        if let Some(auth) = d.get("authentication").filter(|a| a.is_object()) {
            let challenge = auth["challenge"].as_str().unwrap_or("");
            let salt = auth["salt"].as_str().unwrap_or("");
            identify["authentication"] = json!(auth_string(password, salt, challenge));
        }
        ws.send(Message::Text(json!({"op": OP_IDENTIFY, "d": identify}).to_string()))
            .map_err(|e| ObsError::Connect(e.to_string()))?;
        let identified = match read_json(&mut ws) {
            Ok(v) => v,
            // OBS closes the socket with code 4009 on bad auth.
            Err(_) => return Err(ObsError::Auth),
        };
        if identified["op"].as_u64() != Some(OP_IDENTIFIED) {
            return Err(ObsError::Auth);
        }

        ws.get_mut().set_read_timeout(Some(Duration::from_millis(20))).ok();

        let (out_tx, out_rx) = unbounded::<String>();
        let (ev_tx, ev_rx) = unbounded::<ObsEvent>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));

        {
            let pending = pending.clone();
            let alive = alive.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("obs-ws".into())
                .spawn(move || {
                    let reason = run_socket(&mut ws, &out_rx, &ev_tx, &pending, &stop);
                    alive.store(false, Ordering::SeqCst);
                    for (_, (_, tx)) in pending.lock().drain() {
                        let _ = tx.send(Err(ObsError::Disconnected));
                    }
                    let _ = ev_tx.send(ObsEvent::Disconnected(reason));
                    let _ = ws.close(None);
                    let _ = ws.flush();
                })
                .expect("spawn obs-ws thread");
        }

        let client = ObsClient {
            out_tx,
            pending,
            alive,
            stop,
            next_id: AtomicU64::new(1),
            obs_version: String::new(),
            ws_version: obs_ws_version,
            request_timeout: Duration::from_secs(10),
        };
        Ok((client, ev_rx))
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn request(&self, request_type: &str, data: Option<Value>) -> Result<Value, ObsError> {
        self.request_with_timeout(request_type, data, self.request_timeout)
    }

    pub fn request_with_timeout(
        &self,
        request_type: &str,
        data: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, ObsError> {
        if !self.is_alive() {
            return Err(ObsError::Disconnected);
        }
        let id = format!("cv{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        let (tx, rx) = bounded(1);
        self.pending.lock().insert(id.clone(), (request_type.to_string(), tx));
        let mut d = json!({"requestType": request_type, "requestId": id});
        if let Some(data) = data {
            d["requestData"] = data;
        }
        if self.out_tx.send(json!({"op": OP_REQUEST, "d": d}).to_string()).is_err() {
            self.pending.lock().remove(&id);
            return Err(ObsError::Disconnected);
        }
        match rx.recv_timeout(timeout) {
            Ok(r) => r,
            Err(_) => {
                self.pending.lock().remove(&id);
                if self.is_alive() {
                    Err(ObsError::Timeout(request_type.to_string()))
                } else {
                    Err(ObsError::Disconnected)
                }
            }
        }
    }

    /// Convenience: request and pull a string field out of the response.
    pub fn request_str(&self, request_type: &str, data: Option<Value>, field: &str) -> Result<String, ObsError> {
        let v = self.request(request_type, data)?;
        Ok(v[field].as_str().unwrap_or_default().to_string())
    }

    pub fn close(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for ObsClient {
    fn drop(&mut self) {
        self.close();
    }
}

fn read_json(ws: &mut tungstenite::WebSocket<TcpStream>) -> Result<Value, ObsError> {
    loop {
        match ws.read() {
            Ok(Message::Text(t)) => {
                return serde_json::from_str(&t).map_err(|e| ObsError::Connect(e.to_string()));
            }
            Ok(Message::Close(f)) => {
                return Err(ObsError::Connect(format!("closed: {:?}", f.map(|f| f.code))));
            }
            Ok(_) => continue,
            Err(e) => return Err(ObsError::Connect(e.to_string())),
        }
    }
}

fn run_socket(
    ws: &mut tungstenite::WebSocket<TcpStream>,
    out_rx: &Receiver<String>,
    ev_tx: &Sender<ObsEvent>,
    pending: &Pending,
    stop: &AtomicBool,
) -> String {
    loop {
        if stop.load(Ordering::SeqCst) {
            return "closed by client".into();
        }
        while let Ok(msg) = out_rx.try_recv() {
            if let Err(e) = ws.send(Message::Text(msg)) {
                return format!("send failed: {e}");
            }
        }
        match ws.read() {
            Ok(Message::Text(text)) => {
                let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                match v["op"].as_u64() {
                    Some(OP_EVENT) => {
                        let kind = v["d"]["eventType"].as_str().unwrap_or("").to_string();
                        let data = v["d"]["eventData"].clone();
                        let _ = ev_tx.send(ObsEvent::Event { kind, data });
                    }
                    Some(OP_REQUEST_RESPONSE) => {
                        let d = &v["d"];
                        let id = d["requestId"].as_str().unwrap_or("");
                        if let Some((request, tx)) = pending.lock().remove(id) {
                            let status = &d["requestStatus"];
                            let result = if status["result"].as_bool() == Some(true) {
                                Ok(d.get("responseData").cloned().unwrap_or(Value::Null))
                            } else {
                                Err(ObsError::Request {
                                    request,
                                    code: status["code"].as_i64().unwrap_or(0),
                                    comment: status["comment"].as_str().unwrap_or("").to_string(),
                                })
                            };
                            let _ = tx.send(result);
                        }
                    }
                    _ => {}
                }
            }
            Ok(Message::Close(frame)) => {
                return format!("OBS closed the connection ({:?})", frame.map(|f| f.code));
            }
            Ok(_) => {
                let _ = ws.flush();
            }
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
            {
                continue;
            }
            Err(e) => return format!("connection lost: {e}"),
        }
    }
}

/// A small helper describing current OBS output state (from GetRecordStatus etc.).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OutputState {
    pub active: bool,
    pub paused: bool,
    pub duration_ms: i64,
    pub bytes: i64,
}

impl OutputState {
    pub fn from_record_status(v: &Value) -> Self {
        Self {
            active: v["outputActive"].as_bool().unwrap_or(false),
            paused: v["outputPaused"].as_bool().unwrap_or(false),
            duration_ms: v["outputDuration"].as_i64().unwrap_or(0),
            bytes: v["outputBytes"].as_i64().unwrap_or(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A tiny fake obs-websocket server to test the client end to end.
    fn fake_server(password: Option<&'static str>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ws = tungstenite::accept_hdr(stream, |req: &tungstenite::handshake::server::Request, mut resp: tungstenite::handshake::server::Response| {
                assert_eq!(req.headers().get("Sec-WebSocket-Protocol").unwrap(), "obswebsocket.json");
                resp.headers_mut().insert("Sec-WebSocket-Protocol", "obswebsocket.json".parse().unwrap());
                Ok(resp)
            })
            .unwrap();
            let mut hello = json!({"op":0,"d":{"obsWebSocketVersion":"5.5.4","rpcVersion":1}});
            if password.is_some() {
                hello["d"]["authentication"] = json!({"challenge":"chal","salt":"salt"});
            }
            ws.send(Message::Text(hello.to_string())).unwrap();
            let ident: Value = serde_json::from_str(&ws.read().unwrap().into_text().unwrap()).unwrap();
            assert_eq!(ident["op"], 1);
            if let Some(pw) = password {
                if ident["d"]["authentication"] != json!(auth_string(pw, "salt", "chal")) {
                    ws.close(None).ok();
                    let _ = ws.flush();
                    return;
                }
            }
            ws.send(Message::Text(json!({"op":2,"d":{"negotiatedRpcVersion":1}}).to_string())).unwrap();
            // Emit an event, then answer requests until closed.
            ws.send(Message::Text(json!({"op":5,"d":{"eventType":"ReplayBufferSaved","eventIntent":64,"eventData":{"savedReplayPath":"C:/x.mp4"}}}).to_string())).unwrap();
            loop {
                let msg = match ws.read() {
                    Ok(m) => m,
                    Err(_) => return,
                };
                let Message::Text(t) = msg else { continue };
                let v: Value = serde_json::from_str(&t).unwrap();
                let rt = v["d"]["requestType"].as_str().unwrap().to_string();
                let id = v["d"]["requestId"].clone();
                let resp = match rt.as_str() {
                    "GetVersion" => json!({"op":7,"d":{"requestType":rt,"requestId":id,"requestStatus":{"result":true,"code":100},"responseData":{"obsVersion":"31.0.0"}}}),
                    "Hang" => continue,
                    "Quit" => return,
                    _ => json!({"op":7,"d":{"requestType":rt,"requestId":id,"requestStatus":{"result":false,"code":204,"comment":"nope"}}}),
                };
                ws.send(Message::Text(resp.to_string())).unwrap();
            }
        });
        port
    }

    #[test]
    fn connects_requests_and_receives_events() {
        let port = fake_server(Some("pw"));
        let (client, events) = ObsClient::connect(port, "pw", subs::ALL, Duration::from_secs(5)).unwrap();
        assert_eq!(client.ws_version, "5.5.4");
        let v = client.request("GetVersion", None).unwrap();
        assert_eq!(v["obsVersion"], "31.0.0");
        match client.request("Bogus", Some(json!({"a":1}))) {
            Err(ObsError::Request { code, comment, .. }) => {
                assert_eq!(code, 204);
                assert_eq!(comment, "nope");
            }
            other => panic!("unexpected {other:?}"),
        }
        match events.recv_timeout(Duration::from_secs(2)).unwrap() {
            ObsEvent::Event { kind, data } => {
                assert_eq!(kind, "ReplayBufferSaved");
                assert_eq!(data["savedReplayPath"], "C:/x.mp4");
            }
            e => panic!("{e:?}"),
        }
        let r = client.request_with_timeout("Hang", None, Duration::from_millis(200));
        assert_eq!(r, Err(ObsError::Timeout("Hang".into())));
        // Server goes away -> pending requests fail and Disconnected is emitted.
        let _ = client.request_with_timeout("Quit", None, Duration::from_millis(500));
        let mut saw_disconnect = false;
        for _ in 0..50 {
            if let Ok(ObsEvent::Disconnected(_)) = events.recv_timeout(Duration::from_millis(100)) {
                saw_disconnect = true;
                break;
            }
        }
        assert!(saw_disconnect);
        assert!(!client.is_alive());
        assert_eq!(client.request("GetVersion", None), Err(ObsError::Disconnected));
    }

    #[test]
    fn wrong_password_is_auth_error() {
        let port = fake_server(Some("right"));
        let r = ObsClient::connect(port, "wrong", subs::ALL, Duration::from_secs(5));
        assert!(matches!(r, Err(ObsError::Auth)), "{:?}", r.err());
    }

    #[test]
    fn no_server_is_connect_error() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let r = ObsClient::connect(port, "", 0, Duration::from_millis(500));
        assert!(matches!(r, Err(ObsError::Connect(_))));
    }
}

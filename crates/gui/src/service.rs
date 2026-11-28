//! Connection to the background service (auto-reconnecting; starts the
//! service if it isn't running).

use crossbeam_channel::{Receiver, Sender};
use cv_core::ipc::{Event, IpcClient, JobInfo, Meter, Request, Status};
use eframe::egui;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Shared {
    pub status: Option<Status>,
    pub connected: bool,
    pub meters: HashMap<String, f32>,
    pub jobs: HashMap<u64, JobInfo>,
}

/// Events the UI cares about beyond status (library refreshes, toasts).
#[derive(Debug, Clone)]
pub enum UiEvent {
    LibraryChanged,
    RecordingsChanged,
    ClipAdded(i64),
    Toast { level: String, message: String },
    JobDone(JobInfo),
}

pub struct Service {
    toast_tx: Sender<UiEvent>,
    pub shared: Arc<Mutex<Shared>>,
    client: Arc<Mutex<Option<Arc<IpcClient>>>>,
    pub events: Receiver<UiEvent>,
}

impl Service {
    #[cfg(test)]
    pub(crate) fn disconnected_for_test() -> Self {
        let (toast_tx, events) = crossbeam_channel::unbounded();
        Self {
            toast_tx,
            shared: Arc::new(Mutex::new(Shared::default())),
            client: Arc::new(Mutex::new(None)),
            events,
        }
    }

    pub fn start(ctx: egui::Context) -> Service {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let client: Arc<Mutex<Option<Arc<IpcClient>>>> = Arc::new(Mutex::new(None));
        let (tx, rx) = crossbeam_channel::unbounded();
        let tx_keep = tx.clone();
        {
            let shared = shared.clone();
            let client = client.clone();
            std::thread::Builder::new()
                .name("service-conn".into())
                .spawn(move || connection_loop(ctx, shared, client, tx))
                .expect("spawn");
        }
        Service { shared, client, events: rx, toast_tx: tx_keep }
    }

    pub fn status(&self) -> Option<Status> {
        self.shared.lock().status.clone()
    }

    pub fn connected(&self) -> bool {
        self.shared.lock().connected
    }


    /// Send without waiting (slow operations like restarting the recorder). Errors
    /// come back as toasts.
    pub fn fire(&self, req: Request) {
        let c = self.client.lock().clone();
        let tx = self.toast_tx.clone();
        std::thread::spawn(move || {
            let r = match c {
                Some(c) => c.call(req, Duration::from_secs(180)).map_err(|e| format!("{e:#}")),
                None => Err("ClipVault service isn't running yet".into()),
            };
            if let Err(e) = r {
                let _ = tx.send(UiEvent::Toast { level: "error".into(), message: e });
            }
        });
    }

    pub fn call(&self, req: Request) -> Result<serde_json::Value, String> {
        let c = self.client.lock().clone();
        match c {
            Some(c) => c.call(req, Duration::from_secs(20)).map_err(|e| format!("{e:#}")),
            None => Err("ClipVault service isn't running yet".into()),
        }
    }
}

fn start_service_process() {
    let exe = cv_core::paths::exe_dir().join(cv_core::paths::SERVICE_EXE);
    if exe.is_file() {
        tracing::info!("starting service {}", exe.display());
        let _ = std::process::Command::new(&exe).current_dir(cv_core::paths::exe_dir()).spawn();
    }
}

fn connection_loop(ctx: egui::Context, shared: Arc<Mutex<Shared>>, client: Arc<Mutex<Option<Arc<IpcClient>>>>, tx: Sender<UiEvent>) {
    let mut last_spawn: Option<Instant> = None;
    loop {
        match IpcClient::connect(Duration::from_secs(2)) {
            Ok((c, events)) => {
                let c = Arc::new(c);
                *client.lock() = Some(c.clone());
                shared.lock().connected = true;
                let _ = tx.send(UiEvent::LibraryChanged);
                ctx.request_repaint();
                while let Ok(ev) = events.recv() {
                    handle(&ctx, &shared, &tx, ev);
                }
                *client.lock() = None;
                let mut s = shared.lock();
                s.connected = false;
                s.status = None;
                ctx.request_repaint();
            }
            Err(_) => {
                // Service not running: start it (at most every 20 s).
                if last_spawn.map(|t| t.elapsed() > Duration::from_secs(20)).unwrap_or(true) {
                    last_spawn = Some(Instant::now());
                    start_service_process();
                }
            }
        }
        std::thread::sleep(Duration::from_millis(1000));
    }
}

fn handle(ctx: &egui::Context, shared: &Arc<Mutex<Shared>>, tx: &Sender<UiEvent>, ev: Event) {
    match ev {
        Event::Status { status } => {
            let mut s = shared.lock();
            for j in &status.jobs {
                s.jobs.insert(j.id, j.clone());
            }
            s.status = Some(*status);
        }
        Event::Meters { meters } => {
            let mut s = shared.lock();
            for Meter { name, peak } in meters {
                let e = s.meters.entry(name).or_insert(0.0);
                // Fast attack, slow decay.
                *e = if peak > *e { peak } else { *e * 0.85 + peak * 0.15 };
            }
        }
        Event::Job { job } => {
            let finished = job.state == "done" || job.state == "failed" || job.state == "cancelled";
            shared.lock().jobs.insert(job.id, job.clone());
            if finished {
                let _ = tx.send(UiEvent::JobDone(job));
            }
        }
        Event::ClipAdded { clip_id } => {
            let _ = tx.send(UiEvent::ClipAdded(clip_id));
        }
        Event::LibraryChanged => {
            let _ = tx.send(UiEvent::LibraryChanged);
        }
        Event::RecordingsChanged => {
            let _ = tx.send(UiEvent::RecordingsChanged);
        }
        Event::Toast { level, message } => {
            let _ = tx.send(UiEvent::Toast { level, message });
        }
    }
    ctx.request_repaint();
}

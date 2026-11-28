//! Observations of the real worker for the opt-in archive benchmark only.

use super::*;

#[derive(Clone, Copy, Default)]
pub(crate) struct Sample {
    pub chunks: u64,
    pub paused: Option<Instant>,
    pub resumed: Option<Instant>,
}

pub(crate) type Monitor = Arc<parking_lot::Mutex<Sample>>;

thread_local! {
    static MONITOR: std::cell::OnceCell<Monitor> = const { std::cell::OnceCell::new() };
}

pub(crate) fn run(sh: Arc<Shared>, monitor: Monitor) {
    MONITOR.with(|local| assert!(local.set(monitor).is_ok()));
    super::run(sh);
}

pub(super) fn waiting_for_focus() {
    MONITOR.with(|local| {
        if let Some(monitor) = local.get() {
            let mut sample = monitor.lock();
            sample.paused.get_or_insert_with(Instant::now);
            sample.resumed = None;
        }
    });
}

pub(super) fn starting_chunk() {
    MONITOR.with(|local| {
        if let Some(monitor) = local.get() {
            let mut sample = monitor.lock();
            sample.chunks += 1;
            if sample.paused.take().is_some() || sample.resumed.is_none() {
                sample.resumed = Some(Instant::now());
            }
        }
    });
}

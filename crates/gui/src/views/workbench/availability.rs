//! Disk availability for the bin. Rendering only consults the latest cached result.
use cv_core::editor::{Asset, Id};
use eframe::egui;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::mpsc::{self, Receiver, TryRecvError},
    time::{Duration, Instant},
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(15);
type Scan = Vec<(Id, PathBuf, bool)>;

struct Source {
    path: PathBuf,
    available: Option<bool>,
}

#[derive(Default)]
pub(super) struct Availability {
    sources: HashMap<Id, Source>,
    pub(super) missing_summary: Option<(Id, usize)>,
    next_scan: Option<Instant>,
    scan: Option<Receiver<Scan>>,
}

impl Availability {
    pub(super) fn sync(&mut self, assets: &[Asset]) {
        if assets.len() == self.sources.len() && assets.iter().all(|asset| {
            self.sources.get(&asset.id).is_some_and(|source| source.path == asset.path)
        }) {
            return;
        }

        let mut previous = std::mem::take(&mut self.sources);
        self.sources = assets.iter().map(|asset| {
            let source = previous.remove(&asset.id).filter(|old| old.path == asset.path).unwrap_or_else(|| Source {
                path: asset.path.clone(),
                available: None,
            });
            (asset.id, source)
        }).collect();
        self.summarize_missing();
        self.refresh();
    }

    fn summarize_missing(&mut self) {
        let mut missing = self.sources.iter().filter_map(|(&id, source)| (source.available == Some(false)).then_some(id));
        self.missing_summary = missing.next().map(|id| missing.fold((id, 1), |(first, count), next| (first.min(next), count + 1)));
    }

    pub(super) fn refresh(&mut self) {
        self.next_scan = (!self.sources.is_empty()).then(Instant::now);
    }

    pub(super) fn mark_offline(&mut self, id: Id) {
        if let Some(source) = self.sources.get_mut(&id) {
            if source.available != Some(false) {
                source.available = Some(false);
                self.summarize_missing();
                // A scan started before the decoder found the missing file may still say it exists.
                self.scan = None;
                self.refresh();
            }
        }
    }

    pub(super) fn missing(&self, id: Id) -> bool {
        self.sources.get(&id).is_some_and(|source| source.available == Some(false))
    }

    fn take_scan(&mut self, now: Instant) -> Option<Vec<(Id, PathBuf)>> {
        if self.scan.is_some() || self.next_scan.is_none_or(|next| now < next) {
            return None;
        }
        self.next_scan = Some(now + REFRESH_INTERVAL);
        Some(self.sources.iter().map(|(&id, source)| (id, source.path.clone())).collect())
    }

    pub(super) fn poll(&mut self, ctx: &egui::Context) {
        if let Some(task) = &self.scan {
            match task.try_recv() {
                Ok(results) => {
                    self.scan = None;
                    for (id, path, available) in results {
                        if let Some(source) = self.sources.get_mut(&id).filter(|source| source.path == path) {
                            source.available = Some(available);
                        }
                    }
                    self.summarize_missing();
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => self.scan = None,
            }
        }
        let now = Instant::now();
        let Some(paths) = self.take_scan(now) else {
            if self.missing_summary.is_some() {
                if let Some(next) = self.next_scan { ctx.request_repaint_after(next.saturating_duration_since(now)); }
            }
            return;
        };
        let (tx, rx) = mpsc::channel();
        let wake = ctx.clone();
        match std::thread::Builder::new().name("clipvault-editor-bin-scan".into()).spawn(move || {
            let results = paths.into_iter().map(|(id, path)| {
                let available = path.is_file();
                (id, path, available)
            }).collect();
            let _ = tx.send(results);
            wake.request_repaint();
        }) {
            Ok(_) => self.scan = Some(rx),
            Err(error) => tracing::warn!(%error, "could not refresh Editor media availability"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cv_core::editor::{AssetInfo, AssetKind};

    fn asset(path: &str) -> Asset {
        Asset {
            id: 1,
            path: path.into(),
            info: AssetInfo { kind: AssetKind::Video, fps_milli: None, duration: 10_000_000, width: 1280, height: 720 },
        }
    }

    #[test]
    fn newly_offline_source_supersedes_a_scan_started_before_removal() {
        let mut availability = Availability::default();
        availability.sync(&[asset("recording.mp4")]);
        let (tx, rx) = mpsc::channel();
        availability.scan = Some(rx);
        tx.send(vec![(1, "recording.mp4".into(), true)]).unwrap();
        availability.mark_offline(1);
        assert!(availability.scan.is_none());
        assert!(availability.missing(1));
        assert_eq!(availability.missing_summary, Some((1, 1)));
        assert!(availability.take_scan(Instant::now()).is_some());
    }

    #[test]
    fn stable_media_bin_repaints_do_not_reschedule_disk_reads() {
        let mut availability = Availability::default();
        availability.sync(&[asset("footage.mp4")]);
        let now = Instant::now();
        assert_eq!(availability.take_scan(now).unwrap(), vec![(1, PathBuf::from("footage.mp4"))]);

        for frame in 1..=300 {
            assert!(!availability.missing(1));
            assert!(availability.take_scan(now + Duration::from_millis(frame * 16)).is_none());
        }
        assert!(availability.take_scan(now + REFRESH_INTERVAL).is_some());

        availability.sync(&[asset("relinked.mp4")]);
        assert_eq!(availability.take_scan(Instant::now()).unwrap(), vec![(1, PathBuf::from("relinked.mp4"))]);
    }
}

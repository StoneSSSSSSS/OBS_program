//! Asynchronous thumbnail loading into egui textures (LRU-ish cache).

use crossbeam_channel::{Receiver, Sender};
use cv_core::model::Recording;
use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

pub struct Thumbs {
    cache: HashMap<String, TextureHandle>,
    order: VecDeque<String>,
    pending: HashSet<String>,
    failed: HashSet<String>,
    req_tx: Sender<String>,
    res_rx: Receiver<(String, Option<ColorImage>)>,
    recording_tx: Sender<RecordingKey>,
    recording_rx: Receiver<(RecordingKey, Option<ColorImage>)>,
    recording_pending: bool,
    recording: Option<(RecordingKey, Option<TextureHandle>)>,
}

const MAX: usize = 400;

#[derive(PartialEq, Eq)]
struct RecordingKey {
    path: String,
    start_ms: i64,
    end_ms: i64,
}

impl Thumbs {
    pub fn new(ctx: egui::Context) -> Thumbs {
        let (req_tx, req_rx) = crossbeam_channel::unbounded::<String>();
        let (res_tx, res_rx) = crossbeam_channel::unbounded();
        let (recording_tx, recording_req) = crossbeam_channel::bounded::<RecordingKey>(1);
        let (recording_res, recording_rx) = crossbeam_channel::bounded(1);
        for i in 0..2 {
            let req_rx = req_rx.clone();
            let res_tx = res_tx.clone();
            let recording_req = recording_req.clone();
            let recording_res = recording_res.clone();
            let ctx = ctx.clone();
            std::thread::Builder::new()
                .name(format!("thumbs-{i}"))
                .spawn(move || {
                    loop {
                        crossbeam_channel::select! {
                            recv(recording_req) -> request => {
                                let Ok(key) = request else { break };
                                let seconds = (key.end_ms - key.start_ms) as f64 / 1000.0;
                                let img = cv_media::thumb::picture(Path::new(&key.path), seconds * 0.75, 480)
                                    .ok()
                                    .map(|rgb| ColorImage::from_rgb([rgb.width() as usize, rgb.height() as usize], rgb.as_raw()));
                                let _ = recording_res.send((key, img));
                            }
                            recv(req_rx) -> request => {
                                let Ok(path) = request else { break };
                                let img = image::open(&path).ok().map(|i| {
                                    let i = i.to_rgba8();
                                    ColorImage::from_rgba_unmultiplied([i.width() as usize, i.height() as usize], i.as_raw())
                                });
                                let _ = res_tx.send((path, img));
                            }
                        }
                        ctx.request_repaint();
                    }
                })
                .expect("spawn");
        }
        Thumbs {
            cache: HashMap::new(), order: VecDeque::new(), pending: HashSet::new(), failed: HashSet::new(),
            req_tx, res_rx, recording_tx, recording_rx, recording_pending: false, recording: None,
        }
    }

    /// Call once per frame to upload finished decodes.
    pub fn poll(&mut self, ctx: &egui::Context) {
        if let Ok((key, image)) = self.recording_rx.try_recv() {
            self.recording_pending = false;
            self.recording = Some((key, image.map(|image| ctx.load_texture("latest recording", image, TextureOptions::LINEAR))));
        }
        while let Ok((path, img)) = self.res_rx.try_recv() {
            self.pending.remove(&path);
            match img {
                Some(img) => {
                    let tex = ctx.load_texture(&path, img, TextureOptions::LINEAR);
                    self.cache.insert(path.clone(), tex);
                    self.order.push_back(path);
                    while self.order.len() > MAX {
                        if let Some(old) = self.order.pop_front() {
                            self.cache.remove(&old);
                        }
                    }
                }
                None => {
                    self.failed.insert(path);
                }
            }
        }
    }

    /// At most one recording is decoded or cached here, including failed decodes.
    pub fn recording(&mut self, recording: &Recording) -> Option<TextureHandle> {
        let key = RecordingKey { path: recording.path.clone(), start_ms: recording.start_ms, end_ms: recording.end_ms() };
        if let Some((cached, texture)) = &self.recording {
            if *cached == key {
                return texture.clone();
            }
        }
        if !self.recording_pending {
            self.recording_pending = self.recording_tx.try_send(key).is_ok();
        }
        None
    }

    pub fn get(&mut self, path: &str) -> Option<TextureHandle> {
        if path.is_empty() || path == "-" {
            return None;
        }
        if let Some(t) = self.cache.get(path) {
            return Some(t.clone());
        }
        if !self.pending.contains(path) && !self.failed.contains(path) {
            self.pending.insert(path.to_string());
            let _ = self.req_tx.send(path.to_string());
        }
        None
    }
}

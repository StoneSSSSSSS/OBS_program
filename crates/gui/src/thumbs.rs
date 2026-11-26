//! Asynchronous thumbnail loading into egui textures (LRU-ish cache).

use crossbeam_channel::{Receiver, Sender};
use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use std::collections::{HashMap, HashSet, VecDeque};

pub struct Thumbs {
    cache: HashMap<String, TextureHandle>,
    order: VecDeque<String>,
    pending: HashSet<String>,
    failed: HashSet<String>,
    req_tx: Sender<String>,
    res_rx: Receiver<(String, Option<ColorImage>)>,
}

const MAX: usize = 400;

impl Thumbs {
    pub fn new(ctx: egui::Context) -> Thumbs {
        let (req_tx, req_rx) = crossbeam_channel::unbounded::<String>();
        let (res_tx, res_rx) = crossbeam_channel::unbounded();
        for i in 0..2 {
            let req_rx = req_rx.clone();
            let res_tx = res_tx.clone();
            let ctx = ctx.clone();
            std::thread::Builder::new()
                .name(format!("thumbs-{i}"))
                .spawn(move || {
                    for path in req_rx {
                        let img = image::open(&path).ok().map(|i| {
                            let i = i.to_rgba8();
                            ColorImage::from_rgba_unmultiplied([i.width() as usize, i.height() as usize], i.as_raw())
                        });
                        let _ = res_tx.send((path, img));
                        ctx.request_repaint();
                    }
                })
                .expect("spawn");
        }
        Thumbs { cache: HashMap::new(), order: VecDeque::new(), pending: HashSet::new(), failed: HashSet::new(), req_tx, res_rx }
    }

    /// Call once per frame to upload finished decodes.
    pub fn poll(&mut self, ctx: &egui::Context) {
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

    /// Forget a path (e.g. the thumbnail was regenerated).
    pub fn invalidate(&mut self, path: &str) {
        self.cache.remove(path);
        self.failed.remove(path);
    }
}

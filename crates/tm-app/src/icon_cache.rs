//! Shared cache of real process icons (shell-extracted) as egui textures.
//!
//! Extraction is expensive (~ms per icon), so it happens on a dedicated
//! worker thread. The UI thread only enqueues unknown paths and uploads
//! already-decoded results (cheap for 32×32 icons); frames never block.

use eframe::egui;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use tm_core::sync;
use tm_platform::actions::PlatformActions;

#[derive(Default)]
struct Inner {
    /// exe path -> decoded texture (None = extraction failed, don't retry).
    tex: HashMap<String, Option<egui::TextureHandle>>,
    /// Paths queued or awaiting upload (dedup guard).
    queued: HashSet<String>,
}

/// Decoded RGBA result handed over from the worker.
struct Decoded {
    path: String,
    icon: Option<(u32, u32, Vec<u8>)>, // straight-alpha w,h,bytes
}

#[derive(Clone, Default)]
pub struct IconCache {
    inner: Arc<Mutex<Inner>>,
    tx: Option<Arc<Sender<String>>>,
    results: Arc<Mutex<Vec<Decoded>>>,
}

impl IconCache {
    /// Spawn the single extraction worker (idempotent). `actions` is shared
    /// with the worker; it must be `Send + Sync` (it is).
    pub fn start_worker(&mut self, actions: Arc<dyn PlatformActions>) {
        if self.tx.is_some() {
            return;
        }
        let (tx, rx): (Sender<String>, Receiver<String>) = std::sync::mpsc::channel();
        let results = self.results.clone();
        let spawned = std::thread::Builder::new()
            .name("tm-icons".into())
            .spawn(move || icon_worker(rx, actions, results));
        match spawned {
            Ok(_) => self.tx = Some(Arc::new(tx)),
            Err(e) => tracing::warn!(error = %e, "failed to spawn icon worker"),
        }
    }

    /// Look up the texture for `path`; schedules background extraction when
    /// unknown and uploads any freshly decoded results. Never blocks.
    pub fn get(
        &self,
        ctx: &egui::Context,
        _actions: &dyn PlatformActions,
        path: &str,
        _budget: usize,
    ) -> Option<egui::TextureHandle> {
        // 1) Drain finished extractions into textures + cache.
        let ready: Vec<Decoded> = {
            let mut r = sync::lock(&self.results);
            std::mem::take(&mut *r)
        };
        if !ready.is_empty() {
            let mut inner = sync::lock(&self.inner);
            for d in ready {
                let tex = d.icon.map(|(w, h, rgba)| {
                    // GDI returns straight alpha; egui Color32 is premultiplied.
                    let mut premul = rgba;
                    for px in premul.as_chunks_mut::<4>().0 {
                        let a = px[3] as u32;
                        px[0] = ((px[0] as u32 * a + 127) / 255) as u8;
                        px[1] = ((px[1] as u32 * a + 127) / 255) as u8;
                        px[2] = ((px[2] as u32 * a + 127) / 255) as u8;
                    }
                    let img =
                        egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &premul);
                    ctx.load_texture(
                        format!("icon:{}", d.path),
                        img,
                        egui::TextureOptions::LINEAR,
                    )
                });
                inner.queued.remove(&d.path);
                inner.tex.insert(d.path, tex);
            }
        }

        // 2) Serve cached entry or enqueue for the worker.
        {
            let mut inner = sync::lock(&self.inner);
            match inner.tex.get(path) {
                Some(t) => t.clone(),
                None => {
                    if !inner.queued.contains(path) {
                        inner.queued.insert(path.to_string());
                        drop(inner);
                        if let Some(tx) = &self.tx
                            && tx.send(path.to_string()).is_err()
                        {
                            // Worker gone; un-queue so we don't leak the entry.
                            sync::lock(&self.inner).queued.remove(path);
                        }
                    }
                    None
                }
            }
        }
    }
}

fn icon_worker(
    rx: Receiver<String>,
    actions: Arc<dyn PlatformActions>,
    results: Arc<Mutex<Vec<Decoded>>>,
) {
    while let Ok(path) = rx.recv() {
        let icon = actions.process_icon_rgba(&path);
        sync::lock(&results).push(Decoded { path, icon });
    }
}

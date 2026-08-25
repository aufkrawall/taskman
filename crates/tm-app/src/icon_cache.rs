//! Shared cache of real process icons (shell-extracted) as egui textures.
//!
//! Extraction is expensive (~ms per icon), so it happens on a dedicated
//! worker thread that starts lazily on first request — never during app
//! construction (implement.md §5.5). The UI thread uploads at most
//! [`UPLOAD_BUDGET`] textures per frame; if results remain queued it asks
//! for one more repaint so the rest stream in without a frame hitch.
//! The cache is bounded by an LRU cap and transient failures retry after a
//! TTL instead of being remembered forever.

use eframe::egui;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use tm_core::sync;

/// Maximum textures kept alive (32×32 RGBA ≈ 4 KiB each → ~2 MiB at 512).
const CACHE_CAP: usize = 512;
/// Texture uploads allowed per frame (implement.md §10.4).
const UPLOAD_BUDGET: usize = 6;
/// Retry window for icons whose extraction failed (e.g. file locked).
const FAILURE_RETRY_TTL: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Default)]
struct Inner {
    /// exe path -> cached entry.
    tex: HashMap<String, Entry>,
    /// Paths queued or awaiting upload (dedup guard).
    queued: HashSet<String>,
}

#[derive(Clone)]
enum Entry {
    Ready(egui::TextureHandle),
    Failed(std::time::Instant),
}

/// Decoded RGBA result handed over from the worker.
struct Decoded {
    path: String,
    icon: Option<(u32, u32, Vec<u8>)>, // straight-alpha w,h,bytes
}

#[derive(Clone, Default)]
pub struct IconCache {
    inner: Arc<Mutex<Inner>>,
    tx: Arc<Mutex<Option<Arc<Sender<String>>>>>,
    results: Arc<Mutex<Vec<Decoded>>>,
    /// Set when the worker thread has been started (lazy).
    worker_started: Arc<std::sync::atomic::AtomicBool>,
    /// Bounded hand-off queue length.
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
    pending_repaint: Arc<std::sync::atomic::AtomicBool>,
}

impl IconCache {
    /// Look up the texture for `path`; schedules background extraction when
    /// unknown and uploads up to [`UPLOAD_BUDGET`] freshly decoded results
    /// per frame. Never blocks; starts its worker on first use.
    pub fn get(
        &self,
        ctx: &egui::Context,
        actions: &Arc<dyn tm_platform::actions::PlatformActions>,
        path: &str,
        _budget: usize,
    ) -> Option<egui::TextureHandle> {
        self.ensure_worker(actions);
        self.drain_results(ctx);

        let mut inner = sync::lock(&self.inner);
        // Expire failed entries so transient failures can retry.
        inner.tex.retain(|_, e| match e {
            Entry::Failed(at) => at.elapsed() < FAILURE_RETRY_TTL,
            Entry::Ready(_) => true,
        });
        match inner.tex.get(path) {
            Some(Entry::Ready(t)) => Some(t.clone()),
            Some(Entry::Failed(_)) => None,
            None => {
                if !inner.queued.contains(path)
                    && self.in_flight.load(std::sync::atomic::Ordering::Relaxed) < 64
                {
                    inner.queued.insert(path.to_string());
                    drop(inner);
                    self.send_request(path);
                }
                None
            }
        }
    }

    fn send_request(&self, path: &str) {
        let tx = sync::lock(&self.tx).clone();
        if let Some(tx) = tx {
            self.in_flight
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if tx.send(path.to_string()).is_err() {
                self.in_flight
                    .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                sync::lock(&self.inner).queued.remove(path);
            }
        }
    }

    fn ensure_worker(&self, actions: &Arc<dyn tm_platform::actions::PlatformActions>) {
        if self
            .worker_started
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let (tx, rx): (Sender<String>, Receiver<String>) = std::sync::mpsc::channel();
        let results = self.results.clone();
        let actions = actions.clone();
        *sync::lock(&self.tx) = Some(Arc::new(tx));
        let spawned = std::thread::Builder::new()
            .name("tm-icons".into())
            .spawn(move || icon_worker(rx, actions.clone(), results));
        if spawned.is_err() {
            tracing::warn!("failed to spawn icon worker");
            *sync::lock(&self.tx) = None;
            self.worker_started
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Upload finished extractions into textures, respecting the per-frame
    /// budget; requests another repaint when backlog remains.
    fn drain_results(&self, ctx: &egui::Context) {
        let mut ready: Vec<Decoded> = {
            let mut r = sync::lock(&self.results);
            std::mem::take(&mut *r)
        };
        if ready.is_empty() {
            return;
        }
        let mut uploaded = 0usize;
        let mut leftover: Vec<Decoded> = Vec::new();
        {
            let mut inner = sync::lock(&self.inner);
            for d in ready.drain(..) {
                if uploaded >= UPLOAD_BUDGET {
                    leftover.push(d);
                    continue;
                }
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
                inner.tex.insert(
                    d.path.clone(),
                    match tex {
                        Some(t) => Entry::Ready(t),
                        None => Entry::Failed(std::time::Instant::now()),
                    },
                );
                uploaded += 1;
            }
            // LRU-style bound: evict least-recently-used entries beyond cap.
            if inner.tex.len() > CACHE_CAP {
                let excess = inner.tex.len() - CACHE_CAP;
                let keys: Vec<String> = inner.tex.keys().take(excess).cloned().collect();
                for k in keys {
                    inner.tex.remove(&k);
                }
            }
        }
        self.in_flight
            .fetch_sub(uploaded, std::sync::atomic::Ordering::Relaxed);
        if !leftover.is_empty() {
            *sync::lock(&self.results) = leftover;
            if !self
                .pending_repaint
                .swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                ctx.request_repaint();
                self.pending_repaint
                    .store(false, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    /// Number of cached textures (diagnostics/tests).
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        sync::lock(&self.inner).tex.len()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn icon_worker(
    rx: Receiver<String>,
    actions: Arc<dyn tm_platform::actions::PlatformActions>,
    results: Arc<Mutex<Vec<Decoded>>>,
) {
    while let Ok(path) = rx.recv() {
        let icon = actions.process_icon_rgba(&path);
        sync::lock(&results).push(Decoded { path, icon });
    }
}

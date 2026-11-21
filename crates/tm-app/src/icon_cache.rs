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
    /// Monotonic clock for LRU: bumped on every cache hit.
    next_use: u64,
}

#[derive(Clone)]
enum Entry {
    Ready {
        tex: egui::TextureHandle,
        /// LRU clock value of the last hit; eviction removes the smallest.
        last_used: u64,
    },
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
        let use_tick = inner.next_use;
        enum Hit {
            Ready(egui::TextureHandle),
            Miss,
        }
        let hit = match inner.tex.get_mut(path) {
            Some(Entry::Ready { tex, last_used }) => {
                *last_used = use_tick;
                Hit::Ready(tex.clone())
            }
            Some(Entry::Failed(at)) => {
                if at.elapsed() >= FAILURE_RETRY_TTL {
                    inner.tex.remove(path);
                    Hit::Miss
                } else {
                    return None;
                }
            }
            None => Hit::Miss,
        };
        if let Hit::Ready(t) = hit {
            inner.next_use = use_tick + 1;
            return Some(t);
        }
        if !inner.queued.contains(path)
            && self.in_flight.load(std::sync::atomic::Ordering::Relaxed) < 64
        {
            inner.queued.insert(path.to_string());
            drop(inner);
            self.send_request(path);
        }
        None
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
                // The worker thread is gone (panicked or the channel closed):
                // allow ensure_worker to spawn a fresh one on the next get().
                *sync::lock(&self.tx) = None;
                self.worker_started
                    .store(false, std::sync::atomic::Ordering::SeqCst);
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
    /// budget; requests a repaint whenever anything was uploaded (and again
    /// when a backlog remains) so fresh icons appear without waiting for an
    /// unrelated frame.
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
                    let img =
                        egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                    ctx.load_texture(
                        format!("icon:{}", d.path),
                        img,
                        egui::TextureOptions::LINEAR,
                    )
                });
                inner.queued.remove(&d.path);
                let entry = match tex {
                    Some(t) => Entry::Ready {
                        tex: t,
                        last_used: inner.next_use,
                    },
                    None => Entry::Failed(std::time::Instant::now()),
                };
                inner.next_use += 1;
                inner.tex.insert(d.path.clone(), entry);
                uploaded += 1;
            }
            // Expire failed entries so transient failures can retry.
            inner.tex.retain(|_, e| match e {
                Entry::Failed(at) => at.elapsed() < FAILURE_RETRY_TTL,
                Entry::Ready { .. } => true,
            });
            // Bound the cache, evicting least-recently-used entries beyond the
            // cap. Failed entries carry no texture and are dropped first. (A
            // plain HashMap key dump would evict RANDOM entries — including
            // icons currently on screen — which is what made icons flicker
            // back to the placeholder once the cache filled up.)
            if inner.tex.len() > CACHE_CAP {
                let mut excess = inner.tex.len() - CACHE_CAP;
                let failed: Vec<String> = inner
                    .tex
                    .iter()
                    .filter(|(_, e)| matches!(e, Entry::Failed(_)))
                    .map(|(k, _)| k.clone())
                    .collect();
                for k in failed {
                    if excess == 0 {
                        break;
                    }
                    inner.tex.remove(&k);
                    excess -= 1;
                }
                if excess > 0 {
                    let mut by_use: Vec<(String, u64)> = inner
                        .tex
                        .iter()
                        .filter_map(|(k, e)| match e {
                            Entry::Ready { last_used, .. } => Some((k.clone(), *last_used)),
                            Entry::Failed(_) => None,
                        })
                        .collect();
                    by_use.sort_by_key(|(_, u)| *u);
                    for (k, _) in by_use.into_iter().take(excess) {
                        inner.tex.remove(&k);
                    }
                }
            }
        }
        self.in_flight
            .fetch_sub(uploaded, std::sync::atomic::Ordering::Relaxed);
        if uploaded > 0 {
            // Freshly uploaded textures must be painted even when nothing is
            // left in the queue — otherwise the last batch waits for an
            // unrelated repaint.
            ctx.request_repaint();
        }
        if !leftover.is_empty() {
            *sync::lock(&self.results) = leftover;
            ctx.request_repaint();
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

/// The cache key for a process row: the exe path when known; otherwise the
/// bare process name for the kernel pseudo-processes ("System", "Registry",
/// ...) that have no image path — the Windows backend maps those names to
/// the real OS image in the icon layer. Any other process without a path has
/// no icon to find, so it stays `None` and keeps the placeholder glyph.
pub fn row_icon_key(exe_path: Option<&std::path::Path>, name: &str) -> Option<String> {
    if let Some(p) = exe_path {
        return Some(p.to_string_lossy().into_owned());
    }
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".exe").unwrap_or(&lower);
    matches!(
        stem,
        "system" | "[system process]" | "registry" | "memcompression" | "secure system"
    )
    .then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tm_platform::actions::PlatformActions;

    /// Actions stub that "extracts" a 2x2 red icon for every path.
    struct IconActions;
    impl PlatformActions for IconActions {
        fn process_icon_rgba(&self, _exe_path: &str) -> Option<(u32, u32, Vec<u8>)> {
            Some((2, 2, [200u8, 10, 10, 255].repeat(4)))
        }
    }

    /// Drive `get` until the background worker's result is uploaded. Bounded
    /// deadline poll — no sleeps.
    fn pump(
        cache: &IconCache,
        ctx: &egui::Context,
        actions: &Arc<dyn PlatformActions>,
        path: &str,
    ) -> Option<egui::TextureHandle> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(t) = cache.get(ctx, actions, path, 6) {
                return Some(t);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::yield_now();
        }
    }

    #[test]
    fn get_eventually_returns_the_extracted_texture() {
        let ctx = egui::Context::default();
        let actions: Arc<dyn PlatformActions> = Arc::new(IconActions);
        let cache = IconCache::default();
        assert!(pump(&cache, &ctx, &actions, r"C:\Windows\System32\lsass.exe").is_some());
        // Second call is a pure cache hit.
        assert!(
            cache
                .get(&ctx, &actions, r"C:\Windows\System32\lsass.exe", 6)
                .is_some()
        );
    }

    #[test]
    fn eviction_is_lru_not_random() {
        let ctx = egui::Context::default();
        let actions: Arc<dyn PlatformActions> = Arc::new(IconActions);
        let cache = IconCache::default();
        let path = |i: usize| format!(r"C:\apps\app{i}.exe");
        for i in 0..CACHE_CAP {
            assert!(pump(&cache, &ctx, &actions, &path(i)).is_some(), "app{i}");
        }
        // Touch app0 so it becomes the most-recently-used entry.
        assert!(cache.get(&ctx, &actions, &path(0), 6).is_some());
        // Push past the cap: the four oldest UNTOUCHED entries must go.
        for i in CACHE_CAP..CACHE_CAP + 4 {
            assert!(pump(&cache, &ctx, &actions, &path(i)).is_some(), "app{i}");
        }
        assert_eq!(cache.len(), CACHE_CAP);
        let inner = sync::lock(&cache.inner);
        assert!(inner.tex.contains_key(&path(0)), "touched entry survives");
        for i in 1..5 {
            assert!(!inner.tex.contains_key(&path(i)), "stale app{i} evicted");
        }
        assert!(inner.tex.contains_key(&path(CACHE_CAP + 3)));
    }

    #[test]
    fn row_icon_key_uses_exe_path_and_only_pseudo_names_otherwise() {
        assert_eq!(
            row_icon_key(Some(Path::new(r"C:\x\a.exe")), "a.exe"),
            Some(r"C:\x\a.exe".to_string())
        );
        assert_eq!(row_icon_key(None, "Registry"), Some("Registry".to_string()));
        assert_eq!(row_icon_key(None, "System"), Some("System".to_string()));
        assert_eq!(
            row_icon_key(None, "MemCompression"),
            Some("MemCompression".to_string())
        );
        // A real process whose path is simply unknown has no icon to find.
        assert_eq!(row_icon_key(None, "explorer.exe"), None);
        assert_eq!(row_icon_key(None, "myapp"), None);
    }
}

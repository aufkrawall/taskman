//! Shared cache of real process icons (shell-extracted) as egui textures.
//!
//! Extraction is expensive (~ms per icon), so it happens on a dedicated
//! worker thread that starts lazily on first request — never during app
//! construction (implement.md §5.5). The UI thread uploads at most
//! [`UPLOAD_BUDGET`] textures per frame; if results remain queued it asks
//! for one more repaint so the rest stream in without a frame hitch.
//! The cache is bounded by an LRU cap and transient failures retry after a
//! TTL instead of being remembered forever.
//!
//! Every icon the UI paints comes from here ([`IconCache::icon_or_default`]):
//! the row's own application icon when the OS has one, otherwise the platform's
//! default application icon. The UI never draws an icon of its own.

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
/// Cache key for the platform's default application icon. No real path
/// contains a NUL, so this can never collide with one.
const DEFAULT_APP_ICON_KEY: &str = "\u{0}os-default-app-icon";

/// What the cache knows about one key right now.
enum Lookup {
    /// The texture is uploaded and ready to paint.
    Ready(egui::TextureHandle),
    /// Extraction is queued or running; the answer arrives on a later frame.
    Pending,
    /// The platform has no icon for this key.
    Missing,
}

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
    /// The texture to paint for a row whose executable is `path`.
    ///
    /// This is the only icon entry point the pages use, and it never lets the
    /// UI invent an icon:
    ///
    /// * a row with a real executable paints the icon the OS shell has for it;
    /// * a row without one (kernel pseudo-processes such as "System" or
    ///   "Memory Compression") and a row whose image has no icon the shell can
    ///   extract both paint the platform's *default application icon* — the
    ///   same glyph Explorer shows for an application without an icon of its
    ///   own. The platform's icon theme decides, not ours.
    ///
    /// `None` means "nothing honest to paint yet": the row has a path and its
    /// icon is still being extracted. Never blocks; extraction happens on the
    /// worker thread and the texture arrives on a later frame.
    pub fn icon_or_default(
        &self,
        ctx: &egui::Context,
        actions: &Arc<dyn tm_platform::actions::PlatformActions>,
        path: Option<&str>,
    ) -> Option<egui::TextureHandle> {
        let Some(path) = path else {
            // No image at all: ask for the OS default straight away.
            return self.default_app_icon(ctx, actions);
        };
        match self.lookup(ctx, actions, path) {
            Lookup::Ready(tex) => Some(tex),
            Lookup::Missing => self.default_app_icon(ctx, actions),
            // Its own icon is in flight: painting the default here and
            // swapping it a frame later would read as a flicker of the wrong
            // icon, so the row stays empty until the platform answers.
            Lookup::Pending => None,
        }
    }

    /// The platform's default application icon, if it has one.
    fn default_app_icon(
        &self,
        ctx: &egui::Context,
        actions: &Arc<dyn tm_platform::actions::PlatformActions>,
    ) -> Option<egui::TextureHandle> {
        match self.lookup(ctx, actions, DEFAULT_APP_ICON_KEY) {
            Lookup::Ready(tex) => Some(tex),
            Lookup::Pending | Lookup::Missing => None,
        }
    }

    /// Resolve `key` against the cache, scheduling background extraction when
    /// it is unknown and uploading up to [`UPLOAD_BUDGET`] freshly decoded
    /// results per frame. Never blocks; starts its worker on first use.
    fn lookup(
        &self,
        ctx: &egui::Context,
        actions: &Arc<dyn tm_platform::actions::PlatformActions>,
        key: &str,
    ) -> Lookup {
        self.ensure_worker(actions);
        self.drain_results(ctx);

        let mut inner = sync::lock(&self.inner);
        let use_tick = inner.next_use;
        let hit = match inner.tex.get_mut(key) {
            Some(Entry::Ready { tex, last_used }) => {
                *last_used = use_tick;
                Lookup::Ready(tex.clone())
            }
            Some(Entry::Failed(at)) => {
                if at.elapsed() >= FAILURE_RETRY_TTL {
                    inner.tex.remove(key);
                    Lookup::Missing
                } else {
                    return Lookup::Missing;
                }
            }
            None => Lookup::Missing,
        };
        if let Lookup::Ready(_) = hit {
            inner.next_use = use_tick + 1;
            return hit;
        }
        if !inner.queued.contains(key)
            && self.in_flight.load(std::sync::atomic::Ordering::Relaxed) < 64
        {
            inner.queued.insert(key.to_string());
            drop(inner);
            self.send_request(key);
        }
        Lookup::Pending
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
                    let name = if d.path == DEFAULT_APP_ICON_KEY {
                        "icon:os-default-app".to_string()
                    } else {
                        format!("icon:{}", d.path)
                    };
                    ctx.load_texture(name, img, egui::TextureOptions::LINEAR)
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
            // icons currently on screen — which made icons blink out once the
            // cache filled up.)
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
        // The default application icon is a platform property, not a file
        // icon: ask for it directly instead of resolving it as a path.
        let icon = if path == DEFAULT_APP_ICON_KEY {
            actions.default_app_icon_rgba()
        } else {
            actions.process_icon_rgba(&path)
        };
        sync::lock(&results).push(Decoded { path, icon });
    }
}

/// The cache key for a process row: its executable path, when the OS knows it.
///
/// Rows without one are not keyed at all — [`IconCache::icon_or_default`]
/// paints the platform's default application icon for them. Nothing is derived
/// from the process name: a bare name is not a file, and naming an image on the
/// row's behalf (a pseudo-process "Registry" shown as regedit.exe, say) invents
/// an association the OS does not have.
pub fn row_icon_key(exe_path: Option<&std::path::Path>) -> Option<String> {
    exe_path.map(|p| p.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tm_platform::actions::PlatformActions;

    /// The written pixels of a 2x2 icon, so a test can tell which texture the
    /// cache handed back.
    const RED: [u8; 4] = [200, 10, 10, 255];
    const GREEN: [u8; 4] = [10, 200, 10, 255];

    /// Actions stub that "extracts" a 2x2 red icon for every path and a green
    /// default application icon; `None`s the paths in `missing`.
    struct IconActions {
        /// Paths the platform has no icon for.
        missing: Vec<String>,
        /// Whether the platform has a default application icon at all.
        default_icon: Option<[u8; 4]>,
    }

    impl IconActions {
        fn all() -> Self {
            Self {
                missing: Vec::new(),
                default_icon: Some(GREEN),
            }
        }

        fn missing(path: &str) -> Self {
            Self {
                missing: vec![path.to_string()],
                default_icon: Some(GREEN),
            }
        }
    }

    impl PlatformActions for IconActions {
        fn process_icon_rgba(&self, exe_path: &str) -> Option<(u32, u32, Vec<u8>)> {
            (!self.missing.iter().any(|m| m == exe_path)).then(|| (2, 2, RED.repeat(4)))
        }

        fn default_app_icon_rgba(&self) -> Option<(u32, u32, Vec<u8>)> {
            self.default_icon.map(|c| (2, 2, c.repeat(4)))
        }
    }

    /// Drive `icon_or_default` until the background worker's result is
    /// uploaded. Bounded deadline poll — no sleeps.
    fn pump(
        cache: &IconCache,
        ctx: &egui::Context,
        actions: &Arc<dyn PlatformActions>,
        path: Option<&str>,
    ) -> Option<egui::TextureHandle> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(t) = cache.icon_or_default(ctx, actions, path) {
                return Some(t);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::yield_now();
        }
    }

    /// A row with a real executable paints that executable's icon and caches
    /// it: the second lookup is a pure hit.
    #[test]
    fn path_icon_arrives_and_is_cached() {
        let ctx = egui::Context::default();
        let actions: Arc<dyn PlatformActions> = Arc::new(IconActions::all());
        let cache = IconCache::default();
        let exe = r"C:\Windows\System32\lsass.exe";
        assert!(pump(&cache, &ctx, &actions, Some(exe)).is_some());
        let before = cache.len();
        assert!(cache.icon_or_default(&ctx, &actions, Some(exe)).is_some());
        assert_eq!(cache.len(), before, "a hit does not add an entry");
        assert!(
            !sync::lock(&cache.inner)
                .tex
                .contains_key(DEFAULT_APP_ICON_KEY)
        );
    }

    /// A row whose image the platform has no icon for paints the platform's
    /// default application icon — the same texture a row without any image
    /// path gets, never a placeholder of our own.
    #[test]
    fn row_without_an_icon_paints_the_platform_default() {
        let ctx = egui::Context::default();
        let exe = r"C:\Windows\System32\no-icon-image.exe";
        let actions: Arc<dyn PlatformActions> = Arc::new(IconActions::missing(exe));
        let cache = IconCache::default();
        let without_icon = pump(&cache, &ctx, &actions, Some(exe)).expect("default icon");
        let without_path = pump(&cache, &ctx, &actions, None).expect("default icon");
        assert_eq!(
            without_icon.id(),
            without_path.id(),
            "an icon-less image must paint the platform default, not something else"
        );
    }

    /// A row with no image path at all (kernel pseudo-processes) asks for the
    /// platform default straight away instead of never resolving.
    #[test]
    fn row_without_a_path_still_gets_an_icon() {
        let ctx = egui::Context::default();
        let actions: Arc<dyn PlatformActions> = Arc::new(IconActions::all());
        let cache = IconCache::default();
        assert!(pump(&cache, &ctx, &actions, None).is_some());
        assert!(
            sync::lock(&cache.inner)
                .tex
                .contains_key(DEFAULT_APP_ICON_KEY)
        );
    }

    /// A platform without a default icon (mock backend, an XDG theme that ships
    /// none) leaves the cell empty: nothing is drawn in the OS's name.
    #[test]
    fn no_platform_default_means_no_texture() {
        let ctx = egui::Context::default();
        let actions: Arc<dyn PlatformActions> = Arc::new(IconActions {
            missing: Vec::new(),
            default_icon: None,
        });
        let cache = IconCache::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
        while std::time::Instant::now() < deadline {
            assert!(cache.icon_or_default(&ctx, &actions, None).is_none());
            std::thread::yield_now();
        }
    }

    #[test]
    fn eviction_is_lru_not_random() {
        let ctx = egui::Context::default();
        let actions: Arc<dyn PlatformActions> = Arc::new(IconActions::all());
        let cache = IconCache::default();
        let path = |i: usize| format!(r"C:\apps\app{i}.exe");
        for i in 0..CACHE_CAP {
            assert!(
                pump(&cache, &ctx, &actions, Some(&path(i))).is_some(),
                "app{i}"
            );
        }
        // Touch app0 so it becomes the most-recently-used entry.
        assert!(
            cache
                .icon_or_default(&ctx, &actions, Some(&path(0)))
                .is_some()
        );
        // Push past the cap: the four oldest UNTOUCHED entries must go.
        for i in CACHE_CAP..CACHE_CAP + 4 {
            assert!(
                pump(&cache, &ctx, &actions, Some(&path(i))).is_some(),
                "app{i}"
            );
        }
        assert_eq!(cache.len(), CACHE_CAP);
        let inner = sync::lock(&cache.inner);
        assert!(inner.tex.contains_key(&path(0)), "touched entry survives");
        for i in 1..5 {
            assert!(!inner.tex.contains_key(&path(i)), "stale app{i} evicted");
        }
        assert!(inner.tex.contains_key(&path(CACHE_CAP + 3)));
    }

    /// Only a real executable path is a cache key; a bare process name is not a
    /// file and must not be turned into one.
    #[test]
    fn row_icon_key_is_the_exe_path_only() {
        assert_eq!(
            row_icon_key(Some(Path::new(r"C:\x\a.exe"))),
            Some(r"C:\x\a.exe".to_string())
        );
        assert_eq!(row_icon_key(None), None);
    }

    /// End to end against the real Windows shell (no broker, no GUI): a row
    /// with an image gets that image's icon, a row without any gets the OS
    /// default, and an image the shell has no icon for gets the same default —
    /// never a blank cell on a path-less row, never a glyph of ours.
    #[cfg(target_os = "windows")]
    #[test]
    fn real_shell_answers_row_icons_and_the_default() {
        let ctx = egui::Context::default();
        let actions: Arc<dyn PlatformActions> = Arc::new(tm_platform::win::WinActions);
        let cache = IconCache::default();
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let image = format!(r"{system_root}\System32\lsass.exe");
        assert!(
            pump(&cache, &ctx, &actions, Some(&image)).is_some(),
            "a known image has a shell icon"
        );
        let default = pump(&cache, &ctx, &actions, None).expect("the OS has a default app icon");
        let unknown_image = format!(r"{system_root}\System32\no-such-image-xyz.exe");
        let fallback = pump(&cache, &ctx, &actions, Some(&unknown_image))
            .expect("an unresolvable image paints the OS default");
        assert_eq!(
            fallback.id(),
            default.id(),
            "an image the shell cannot resolve must paint the OS default icon"
        );
    }
}

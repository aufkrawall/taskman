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
const FAILURE_CACHE_CAP: usize = 512;
/// Texture uploads allowed per frame (implement.md §10.4).
const UPLOAD_BUDGET: usize = 6;
/// Retry window for icons whose extraction failed (e.g. file locked).
const FAILURE_RETRY_TTL: std::time::Duration = std::time::Duration::from_secs(30);
/// Backoff between failed icon-worker spawn attempts. Every visible row asks
/// for its icon on every frame, so an unconditional retry meant a `spawn`
/// syscall, a channel allocation and a `tracing::warn!` per row per frame while
/// the OS kept refusing threads — exactly the hot-path noise this app bans.
const SPAWN_RETRY_BACKOFF_MS: u64 = 30_000;
/// Cache key for the platform's default application icon. No real path
/// contains a NUL, so this can never collide with one.
const DEFAULT_APP_ICON_KEY: &str = "\u{0}os-default-app-icon";

/// Monotonic milliseconds since the first icon request (no wall-clock jumps).
fn monotonic_ms() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

/// Whether a worker spawn may be attempted at `now_ms`, given the deadline
/// stored for the previous failure (`0` = never failed, so always due).
fn spawn_attempt_due(deadline_ms: u64, now_ms: u64) -> bool {
    now_ms >= deadline_ms
}

/// Deadline for the next spawn attempt after a failure at `now_ms`.
fn spawn_retry_deadline(now_ms: u64) -> u64 {
    now_ms.saturating_add(SPAWN_RETRY_BACKOFF_MS)
}

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
    tex: HashMap<String, CachedTexture>,
    failed: HashMap<String, std::time::Instant>,
    /// Paths queued or awaiting upload (dedup guard).
    queued: HashSet<String>,
    /// Monotonic clock for LRU: bumped on every cache hit.
    next_use: u64,
}

struct CachedTexture {
    tex: egui::TextureHandle,
    last_used: u64,
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
    /// Set by the worker when it publishes a result and cleared when
    /// `drain_results` takes the queue, so a lookup on a row with nothing
    /// pending costs one atomic load instead of three mutex acquisitions.
    has_results: Arc<std::sync::atomic::AtomicBool>,
    /// Set when the worker thread has been started (lazy).
    worker_started: Arc<std::sync::atomic::AtomicBool>,
    /// Monotonic deadline before which a failed worker spawn is not retried.
    spawn_deadline_ms: Arc<std::sync::atomic::AtomicU64>,
    /// Bounded hand-off queue length.
    in_flight: Arc<std::sync::atomic::AtomicUsize>,
    /// `(egui pass number, textures uploaded in that pass)`. The budget is
    /// per frame, not per row: `drain_results` runs once for every visible
    /// row, so a per-call counter let one cold frame upload everything.
    budget: Arc<Mutex<(u64, usize)>>,
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
        let Some(path) = path.and_then(icon_lookup_key) else {
            // No image at all (or only a blank one persisted by an older
            // build): ask for the OS default straight away.
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
        if let Some(cached) = inner.tex.get_mut(key) {
            cached.last_used = use_tick;
            let tex = cached.tex.clone();
            inner.next_use += 1;
            return Lookup::Ready(tex);
        }
        if let Some(at) = inner.failed.get(key) {
            if at.elapsed() < FAILURE_RETRY_TTL {
                return Lookup::Missing;
            }
            inner.failed.remove(key);
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

    /// Hand `path` to the worker, or forget it again.
    ///
    /// `queued` is the dedup guard, so a request that never reached the worker
    /// must not stay in it: every row asked for in that frame would then wait
    /// for an answer nobody is working on and paint nothing for the rest of the
    /// process's life (a transient thread-spawn failure, or a worker that died
    /// between two frames).
    fn send_request(&self, path: &str) {
        let tx = sync::lock(&self.tx).clone();
        let sent = tx.is_some_and(|tx| {
            self.in_flight
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if tx.send(path.to_string()).is_ok() {
                return true;
            }
            // The worker thread is gone (panicked or the channel closed). Every
            // request it still owed an answer for died with it, so the whole
            // outstanding set is released: a stale `queued` entry would never be
            // asked for again, and a stale in-flight count would block every
            // later request once it reached the cap.
            *sync::lock(&self.tx) = None;
            sync::lock(&self.inner).queued.clear();
            self.in_flight
                .store(0, std::sync::atomic::Ordering::Relaxed);
            self.worker_started
                .store(false, std::sync::atomic::Ordering::SeqCst);
            false
        });
        if !sent {
            // Also covers "no channel at all": the key must not stay in the
            // dedup set, or its row never asks again.
            sync::lock(&self.inner).queued.remove(path);
        }
    }

    fn ensure_worker(&self, actions: &Arc<dyn tm_platform::actions::PlatformActions>) {
        // Fast path: once the worker runs, every row would otherwise pay an
        // atomic read-modify-write and a clock read here.
        if self
            .worker_started
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let now = monotonic_ms();
        if !spawn_attempt_due(
            self.spawn_deadline_ms
                .load(std::sync::atomic::Ordering::Relaxed),
            now,
        ) {
            // A spawn failed recently: one attempt per backoff window, not one
            // per queued row per frame.
            return;
        }
        if self
            .worker_started
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }
        let (tx, rx): (Sender<String>, Receiver<String>) = std::sync::mpsc::channel();
        let results = self.results.clone();
        let has_results = self.has_results.clone();
        let actions = actions.clone();
        *sync::lock(&self.tx) = Some(Arc::new(tx));
        let spawned = std::thread::Builder::new()
            .name("tm-icons".into())
            .spawn(move || icon_worker(rx, actions.clone(), results, has_results));
        if spawned.is_err() {
            self.spawn_deadline_ms.store(
                spawn_retry_deadline(now),
                std::sync::atomic::Ordering::Relaxed,
            );
            tracing::warn!("failed to spawn the icon worker; retrying after backoff");
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
        // Fast path: `lookup` calls this once per visible row, and a row would
        // otherwise pay three mutex acquisitions with nothing to drain.
        if !self.has_results.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let pass = ctx.cumulative_pass_nr();
        let mut budget = sync::lock(&self.budget);
        if budget.0 == pass && budget.1 >= UPLOAD_BUDGET {
            // This frame's budget is spent; do not drain/requeue the whole
            // backlog for every remaining row.
            return;
        }
        if budget.0 != pass {
            *budget = (pass, 0);
        }
        // Cleared before taking: a result published in between must be seen on
        // the next pass, never lost. (Clearing after the take can swallow it.)
        self.has_results
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let mut ready: Vec<Decoded> = {
            let mut r = sync::lock(&self.results);
            std::mem::take(&mut *r)
        };
        if ready.is_empty() {
            return;
        }
        let remaining = UPLOAD_BUDGET.saturating_sub(budget.1);
        let mut loaded = 0usize;
        let mut processed = 0usize;
        let mut leftover: Vec<Decoded> = Vec::new();
        {
            let mut inner = sync::lock(&self.inner);
            for d in ready.drain(..) {
                if loaded >= remaining {
                    leftover.push(d);
                    continue;
                }
                let tex = d.icon.and_then(|(w, h, rgba)| {
                    let expected = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
                    if rgba.len() != expected {
                        // A platform decoder handed us a buffer the texture
                        // type would assert on (and release builds abort).
                        // Treat it as an extraction failure, never a panic.
                        tracing::warn!(
                            path = %d.path,
                            w,
                            h,
                            len = rgba.len(),
                            "icon decoder returned a mismatched RGBA buffer"
                        );
                        return None;
                    }
                    let img =
                        egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                    let name = if d.path == DEFAULT_APP_ICON_KEY {
                        "icon:os-default-app".to_string()
                    } else {
                        format!("icon:{}", d.path)
                    };
                    Some(ctx.load_texture(name, img, egui::TextureOptions::LINEAR))
                });
                inner.queued.remove(&d.path);
                if let Some(tex) = tex {
                    let last_used = inner.next_use;
                    inner.next_use += 1;
                    inner.tex.insert(d.path, CachedTexture { tex, last_used });
                    loaded += 1;
                } else {
                    inner.failed.insert(d.path, std::time::Instant::now());
                }
                processed += 1;
            }
            inner
                .failed
                .retain(|_, at| at.elapsed() < FAILURE_RETRY_TTL);
            if inner.failed.len() > FAILURE_CACHE_CAP {
                let excess = inner.failed.len() - FAILURE_CACHE_CAP;
                let mut oldest: Vec<_> = inner
                    .failed
                    .iter()
                    .map(|(k, &at)| (k.clone(), at))
                    .collect();
                oldest.sort_by_key(|&(_, at)| at);
                for (key, _) in oldest.into_iter().take(excess) {
                    inner.failed.remove(&key);
                }
            }
            if inner.tex.len() > CACHE_CAP {
                let excess = inner.tex.len() - CACHE_CAP;
                let mut by_use: Vec<_> = inner
                    .tex
                    .iter()
                    .map(|(k, cached)| (k.clone(), cached.last_used))
                    .collect();
                by_use.sort_by_key(|&(_, last_used)| last_used);
                for (key, _) in by_use.into_iter().take(excess) {
                    inner.tex.remove(&key);
                }
            }
        }
        budget.1 += loaded;
        drop(budget);
        // Saturating: a dead worker resets the counter, and the results it had
        // already published are drained after that reset.
        let _ = self.in_flight.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |in_flight| Some(in_flight.saturating_sub(processed)),
        );
        if loaded > 0 {
            // Freshly uploaded textures must be painted even when nothing is
            // left in the queue — otherwise the last batch waits for an
            // unrelated repaint.
            ctx.request_repaint();
        }
        if !leftover.is_empty() {
            // The worker can publish more results while we upload this batch.
            let mut results = sync::lock(&self.results);
            leftover.append(&mut results);
            *results = leftover;
            drop(results);
            // Re-arm the fast path: these entries wait for the next pass.
            self.has_results
                .store(true, std::sync::atomic::Ordering::Relaxed);
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
    has_results: Arc<std::sync::atomic::AtomicBool>,
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
        has_results.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The cache key for a process row: its executable path, when the OS knows one.
///
/// Rows without one are not keyed at all — [`IconCache::icon_or_default`]
/// paints the platform's default application icon for them. Nothing is derived
/// from the process name: a bare name is not a file, and naming an image on the
/// row's behalf (a pseudo-process "Registry" shown as regedit.exe, say) invents
/// an association the OS does not have.
pub fn row_icon_key(exe_path: Option<&std::path::Path>) -> Option<String> {
    let path = exe_path?;
    usable_icon_path(&path.to_string_lossy()).map(str::to_string)
}

fn icon_lookup_key(path: &str) -> Option<&str> {
    #[cfg(target_os = "linux")]
    {
        // XDG resolves bare commands from Startup entries through desktop files.
        let path = path.trim();
        (!path.is_empty()).then_some(path)
    }
    #[cfg(not(target_os = "linux"))]
    usable_icon_path(path)
}

/// Whether `path` can name an application's image at all.
///
/// Seen through the shell, an empty name is the CURRENT DIRECTORY (its folder
/// icon) and a bare name is resolved against the working directory, so
/// anything without a separator — a kernel pseudo-process's `MemCompression`,
/// `Registry`, or a blank string persisted by an older build — is no icon
/// source and must fall through to the platform's default application icon.
pub fn usable_icon_path(path: &str) -> Option<&str> {
    let path = path.trim();
    (!path.is_empty() && path.contains(['\\', '/'])).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::Ordering;
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

    /// Publish a decoded result the way the worker does: the payload plus the
    /// flag that makes `drain_results` look at the queue.
    fn publish(cache: &IconCache, path: &str, icon: Option<(u32, u32, Vec<u8>)>) {
        sync::lock(&cache.results).push(Decoded {
            path: path.to_string(),
            icon,
        });
        cache
            .has_results
            .store(true, std::sync::atomic::Ordering::Relaxed);
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
            // The upload budget is per egui pass; advance one so a backlog
            // larger than the budget can drain while a test waits.
            let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
            out.textures_delta.clear();
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

    #[test]
    fn failed_lookup_keeps_its_retry_delay_when_texture_cache_is_full() {
        let ctx = egui::Context::default();
        let cache = IconCache::default();
        let texture = ctx.load_texture(
            "test",
            egui::ColorImage::filled([1, 1], egui::Color32::WHITE),
            egui::TextureOptions::LINEAR,
        );
        {
            let mut inner = sync::lock(&cache.inner);
            for i in 0..CACHE_CAP {
                inner.tex.insert(
                    format!("/apps/{i}"),
                    CachedTexture {
                        tex: texture.clone(),
                        last_used: i as u64,
                    },
                );
            }
        }
        let path = "/apps/missing";
        cache
            .in_flight
            .store(1, std::sync::atomic::Ordering::Relaxed);
        publish(&cache, path, None);
        let actions: Arc<dyn PlatformActions> = Arc::new(IconActions::missing(path));
        assert!(matches!(
            cache.lookup(&ctx, &actions, path),
            Lookup::Missing
        ));
        assert_eq!(cache.len(), CACHE_CAP);
        let inner = sync::lock(&cache.inner);
        assert!(
            inner.failed.contains_key(path),
            "a failed extraction must be remembered, not re-queued"
        );
        assert!(!inner.queued.contains(path));
        assert!(
            !inner.tex.contains_key(path),
            "a failure must not become a texture"
        );
        drop(inner);
        assert_eq!(
            cache.in_flight.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn upload_budget_is_per_frame_not_per_row() {
        let ctx = egui::Context::default();
        let cache = IconCache::default();
        for i in 0..(2 * UPLOAD_BUDGET) {
            publish(
                &cache,
                &format!("/apps/{i}"),
                Some((1, 1, vec![255, 0, 0, 255])),
            );
        }
        // `lookup` calls this once per visible row; the budget must not reset
        // between those calls.
        cache.drain_results(&ctx);
        assert_eq!(cache.len(), UPLOAD_BUDGET);
        cache.drain_results(&ctx);
        assert_eq!(
            cache.len(),
            UPLOAD_BUDGET,
            "a second row in the same pass must not upload another batch"
        );
        // The next pass may spend the budget again.
        let mut out = ctx.run_ui(egui::RawInput::default(), |_| {});
        out.textures_delta.clear();
        cache.drain_results(&ctx);
        assert_eq!(cache.len(), 2 * UPLOAD_BUDGET);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn startup_command_can_resolve_an_xdg_icon_by_basename() {
        let ctx = egui::Context::default();
        let actions: Arc<dyn PlatformActions> = Arc::new(IconActions::all());
        let cache = IconCache::default();
        assert!(pump(&cache, &ctx, &actions, Some("firefox")).is_some());
        assert!(sync::lock(&cache.inner).tex.contains_key("firefox"));
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

    /// A request that never reached the worker must not stay in the dedup set:
    /// the stuck key would never be asked for again, so that row kept an empty
    /// icon cell for the rest of the process's life instead of retrying.
    #[test]
    fn unsent_request_is_not_left_queued() {
        let cache = IconCache::default();
        let path = r"C:\apps\never-sent.exe";
        sync::lock(&cache.inner).queued.insert(path.to_string());
        // No worker channel at all: the request cannot be delivered.
        cache.send_request(path);
        let inner = sync::lock(&cache.inner);
        assert!(
            !inner.queued.contains(path),
            "the dead request must be forgotten"
        );
        assert_eq!(
            cache.in_flight.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a request that was never handed over is not in flight"
        );
    }

    /// A worker that died between two frames strands every request it still
    /// owed an answer for. The whole outstanding set is released: a stale
    /// `queued` entry would never be asked for again, and a stale in-flight
    /// count would block every later request once it reached the cap.
    #[test]
    fn dead_worker_releases_outstanding_requests() {
        let cache = IconCache::default();
        // A channel whose worker is already gone: `send` answers Err.
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        drop(rx);
        *sync::lock(&cache.tx) = Some(Arc::new(tx));
        cache.worker_started.store(true, Ordering::SeqCst);
        cache.in_flight.store(2, Ordering::Relaxed);
        {
            let mut inner = sync::lock(&cache.inner);
            for key in ["/apps/a", "/apps/b", "/apps/c"] {
                inner.queued.insert(key.to_string());
            }
        }
        cache.send_request("/apps/c");
        assert!(sync::lock(&cache.inner).queued.is_empty());
        assert_eq!(cache.in_flight.load(Ordering::Relaxed), 0);
        assert!(!cache.worker_started.load(Ordering::SeqCst));
    }

    /// The in-flight counter is decremented saturating: a worker that died and
    /// released the counter must not have it wrapped back to `usize::MAX` by the
    /// results it published before dying (that would block every later request).
    #[test]
    fn drain_results_does_not_wrap_the_in_flight_counter() {
        let ctx = egui::Context::default();
        let cache = IconCache::default();
        cache.in_flight.store(1, Ordering::Relaxed);
        for i in 0..3 {
            publish(
                &cache,
                &format!("/apps/{i}"),
                Some((1, 1, vec![255, 0, 0, 255])),
            );
        }
        cache.drain_results(&ctx);
        assert_eq!(cache.in_flight.load(Ordering::Relaxed), 0);
        assert_eq!(cache.len(), 3, "every published icon is still uploaded");
    }

    /// The spawn backoff is what keeps a failing spawn from being retried for
    /// every queued row on every frame (thread spawn, syscalls and a warning log
    /// per row per frame). `0` means "never failed".
    #[test]
    fn spawn_backoff_skips_attempts_until_the_deadline_passes() {
        let failed_at = 1_000;
        let deadline = spawn_retry_deadline(failed_at);
        assert_eq!(deadline, failed_at + SPAWN_RETRY_BACKOFF_MS);
        assert!(!spawn_attempt_due(deadline, failed_at));
        assert!(!spawn_attempt_due(deadline, deadline - 1));
        assert!(spawn_attempt_due(deadline, deadline));
        assert!(spawn_attempt_due(0, 0), "the first attempt is always due");
        assert_eq!(
            spawn_retry_deadline(u64::MAX),
            u64::MAX,
            "the deadline saturates instead of wrapping into the past"
        );
    }

    /// The control case: a delivered request stays queued (so no duplicate is
    /// sent) and counts as in flight until its result is uploaded.
    #[test]
    fn delivered_request_stays_queued_until_answered() {
        let actions: Arc<dyn PlatformActions> = Arc::new(IconActions::all());
        let cache = IconCache::default();
        cache.ensure_worker(&actions);
        let path = r"C:\apps\queued.exe";
        sync::lock(&cache.inner).queued.insert(path.to_string());
        cache.send_request(path);
        assert!(sync::lock(&cache.inner).queued.contains(path));
        assert_eq!(
            cache.in_flight.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    /// A blank path and a bare process name are not image paths. Asking the
    /// shell for one makes it answer with the current directory's folder icon
    /// (the folder glyph App History showed for path-less apps), so they count
    /// as "no icon of its own".
    #[test]
    fn blank_and_bare_icon_paths_are_not_asked_for() {
        for path in ["", "   ", "cmd.exe", "MemCompression"] {
            assert_eq!(row_icon_key(Some(Path::new(path))), None, "{path:?}");
            assert_eq!(usable_icon_path(path), None, "{path:?}");
        }
        assert_eq!(
            usable_icon_path(r"C:\Windows\System32\cmd.exe"),
            Some(r"C:\Windows\System32\cmd.exe")
        );
        assert_eq!(usable_icon_path(r"   C:\x\a.exe   "), Some(r"C:\x\a.exe"));

        // …and such a row paints the platform's default icon, keyed nowhere
        // near the blank string.
        let ctx = egui::Context::default();
        let actions: Arc<dyn PlatformActions> = Arc::new(IconActions::all());
        let cache = IconCache::default();
        let blank = pump(&cache, &ctx, &actions, Some("")).expect("default icon");
        let default = pump(&cache, &ctx, &actions, None).expect("default icon");
        assert_eq!(blank.id(), default.id());
        assert!(!sync::lock(&cache.inner).tex.contains_key(""));
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
        let known =
            pump(&cache, &ctx, &actions, Some(&image)).expect("a known image has a shell icon");
        let default = pump(&cache, &ctx, &actions, None).expect("the OS has a default app icon");
        let unknown_image = format!(r"{system_root}\System32\no-such-image-xyz.exe");
        let fallback = pump(&cache, &ctx, &actions, Some(&unknown_image))
            .expect("an unresolvable image paints the OS default");
        assert_eq!(
            fallback.id(),
            default.id(),
            "an image the shell cannot resolve must paint the OS default icon"
        );
        assert_ne!(
            known.id(),
            default.id(),
            "the known image's own icon must not silently be the OS default"
        );
    }
}

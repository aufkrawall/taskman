//! Shared cache of real process icons (shell-extracted) as egui textures.
//! Extraction is expensive (~ms), so it happens lazily with a per-frame
//! budget and results are cached for the whole session.

use eframe::egui;
use parking_lot::Mutex as PlMutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use tm_platform::actions::PlatformActions;

#[derive(Default)]
struct Inner {
    /// exe path -> decoded texture (None = extraction failed, don't retry).
    tex: HashMap<String, Option<egui::TextureHandle>>,
    /// Paths waiting for extraction.
    pending: VecDeque<String>,
    queued: HashSet<String>,
}

#[derive(Clone, Default)]
pub struct IconCache {
    inner: Arc<PlMutex<Inner>>,
}

impl IconCache {
    /// Look up the texture for `path`; schedules extraction when unknown.
    /// Processes at most `budget` extractions per call to keep frames smooth.
    pub fn get(
        &self,
        ctx: &egui::Context,
        actions: &dyn PlatformActions,
        path: &str,
        budget: usize,
    ) -> Option<egui::TextureHandle> {
        {
            let mut inner = self.inner.lock();
            match inner.tex.get(path) {
                Some(t) => return t.clone(),
                None => {
                    if !inner.queued.contains(path) {
                        inner.queued.insert(path.to_string());
                        inner.pending.push_back(path.to_string());
                    }
                }
            }
        }
        // Extraction budget per frame.
        for _ in 0..budget {
            let next = self.inner.lock().pending.pop_front();
            let Some(path) = next else { break };
            let decoded = actions.process_icon_rgba(&path);
            let tex = decoded.map(|(w, h, rgba)| {
                // GDI returns straight alpha; egui Color32 is premultiplied.
                let mut premul = rgba;
                for px in premul.as_chunks_mut::<4>().0 {
                    let a = px[3] as u32;
                    px[0] = ((px[0] as u32 * a + 127) / 255) as u8;
                    px[1] = ((px[1] as u32 * a + 127) / 255) as u8;
                    px[2] = ((px[2] as u32 * a + 127) / 255) as u8;
                }
                egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &premul)
            });
            let tex = tex.map(|img| {
                ctx.load_texture(format!("icon:{path}"), img, egui::TextureOptions::LINEAR)
            });
            let mut inner = self.inner.lock();
            inner.tex.insert(path.clone(), tex);
            inner.queued.remove(&path);
        }
        self.inner.lock().tex.get(path).cloned().flatten()
    }
}

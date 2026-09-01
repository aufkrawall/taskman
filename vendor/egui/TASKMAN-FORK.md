# taskman's egui fork

This tree is a vendored copy of [emilk/egui](https://github.com/emilk/egui) at tag
**0.36.1** (`05297424601afdf5966e5e01c8c63eef2696df54`), added with

```
git subtree add --prefix=vendor/egui https://github.com/emilk/egui 0.36.1 --squash
```

It exists because two things taskman needs are not reachable from stock egui:

1. **Sub-pixel (ClearType) text.** Per-channel glyph coverage needs per-channel alpha
   blending. On a GPU that means dual-source blending, which egui's backends do not have
   (upstream `emilk/egui#2639`). On a CPU it is a per-channel multiply.
2. **A native CPU renderer.** eframe offers only glow and wgpu; `RenderMode::Software`
   used to mean WARP, a D3D12 driver emulated on the CPU at ~14 cores and 2.9 fps.

Both are served by one new backend, so they are one change.

The full design, the blend math, and the verification plan live in
`llm-wiki/render-pipeline.md` in the parent repo. This file is only the *inventory* of what
diverges from upstream, so a rebase is a checklist rather than an investigation.

## Ground rules

- **New files over edits.** New directories never conflict in a subtree merge. Every edit
  to a file that upstream also maintains is permanent rebase surface and must earn its place
  here.
- **The fork must not depend on the `windows` crate.** All platform knowledge (DirectWrite,
  `SystemParametersInfo`, DPI awareness) lives in the parent repo's `tm-app` / `tm-platform`
  and reaches the fork through a plain trait and plain structs. This is what keeps the
  epaint change plausibly upstreamable.
- Nothing here may change behaviour when the new features are switched off. The grayscale /
  GPU paths must stay byte-identical to upstream.

## Divergence inventory

Ordered as the commits that introduce them.

### 0. Build hygiene (droppable — expect upstream to fix these)

Cargo applies `--cap-lints allow` to registry dependencies but **not** to path
dependencies. Vendoring therefore surfaces upstream warnings in every taskman build. Two
were loud enough to drown our own output:

| File | Change | Why |
| --- | --- | --- |
| `crates/egui/src/containers/scene.rs:79` | `f32::EPSILON` → `<f32>::EPSILON` | The bare path resolves to the deprecated `std::f32::EPSILON` module constant rather than the associated const. |
| `crates/egui-wgpu/src/lib.rs` | added `#![recursion_limit = "256"]` | wgpu 30's `Global` type graph exceeds the default 128-step auto-trait solver budget. rustc is phasing this warning into a hard error (rust-lang/rust#159228), so this is defensive, not cosmetic. |

**Drop both on any rebase where upstream has fixed them.** They are marked in-source with
`TASKMAN-FORK:` where a comment fits.

### 1. `epaint`: reach the tessellator from outside

Two accessors upstream simply forgot. `Tessellator::new` is public and takes the atlas
size and its prepared discs, but neither was reachable from outside epaint, so that public
constructor could not actually be called. `egui_software` needs it to tessellate the
non-text shapes itself.

| File | Change | Why |
| --- | --- | --- |
| `crates/epaint/src/lib.rs` | re-export `PreparedDisc` | it appears in `Tessellator::new`s signature but was unnameable |
| `crates/epaint/src/text/fonts.rs` | add `FontsImpl::texture_atlas()` | `FontsView.fonts` is public but the atlas behind it was not |

Both are upstreamable as plain oversight fixes; drop them if upstream adds equivalents.

### 2. `epaint`: opt-in sub-pixel glyph rasterization

*(not yet implemented — this section is the contract)*

| File | Nature |
| --- | --- |
| `crates/epaint/src/text/rasterizer.rs` | **new** — `GlyphRasterizer` trait, `GlyphRequest`, `GlyphBitmap` |
| `crates/epaint/src/text/subpixel.rs` | **new** — `SubpixelMode`, LCD filter, gamma/contrast model |
| `crates/epaint/src/text/font.rs` | **edit** — one hunk in `allocate_glyph_uncached` |
| `crates/epaint/src/text/mod.rs` | **edit** — new `TextOptions` fields |
| `crates/epaint/src/texture_atlas.rs` | **edit** — keep `prepared_discs` on the grayscale ramp |
| `crates/epaint/src/lib.rs` | **edit** — re-exports |

The `font.rs` hunk must be shaped as

```rust
if opts.subpixel.is_off() {
    // ...byte-identical upstream body...
} else {
    self.rasterize_subpixel(/* ... */)
}
```

so the conflict, when it comes, is one block and not a merge of two rasterizers.

### 3. `egui_software`: the CPU painter

`crates/egui_software/` — **entirely new, zero rebase surface.**

### 4. `eframe`: `Renderer::Software`

| File | Nature |
| --- | --- |
| `crates/eframe/src/native/software_integration.rs` | **new**, but a *derived copy* — see below |
| `crates/eframe/src/epi.rs` | **edit** — one enum variant + `Default`/`Display`/`FromStr` arms |
| `crates/eframe/src/lib.rs` | **edit** — dispatch arms |
| `crates/eframe/src/native/mod.rs` | **edit** — one `mod` line |
| `crates/eframe/src/native/run.rs` | **edit** — `run_software{,_and_return}` |
| `crates/eframe/Cargo.toml` | **edit** — `softbuffer` dep + `software` feature |

**Deliberate limitation: the software backend supports the ROOT VIEWPORT ONLY.** Most of
`glow_integration.rs` is multi-window machinery -- deferred viewports, immediate
viewports, per-viewport surfaces, and parent/child repaint routing. Child viewports are
logged and ignored rather than half-supported; a partial implementation of viewport
lifetime is the kind of thing that works in testing and strands a window on someone's
desktop in production. `ViewportCommand`s acting on the root window (title, visibility,
close, decorations, always-on-top) are handled in full through `egui_winit`. taskman uses
no child viewports.

`software_integration.rs` is adapted from `glow_integration.rs` with glutin/GL removed. It
is the single largest recurring maintenance cost in this fork: upstream fixes to viewport
lifetime, AccessKit teardown and immediate-viewport handling land in glow's copy and must be
ported by hand. It therefore carries a provenance header:

```rust
//! Derived from eframe's `glow_integration.rs`.
//! UPSTREAM_BASE: <sha of glow_integration.rs at the last rebase>
```

**Three things in that file are easy to get wrong and produce silent breakage.** All
three were shipped broken once and are called out here so a future port does not repeat
them:

- **`egui_winit` does not raise the close event.** `WindowEvent::CloseRequested` only
  returns `repaint: true`; the backend must push `egui::ViewportEvent::Close` into its own
  `ViewportInfo`, or the app never learns the user clicked the close button. taskman vetoes
  the close and hides to the tray, so this is the path that feature runs on.
- **Return `EventResult::CloseRequested`, not `Exit`.** The wrapper runs `save_and_destroy`
  only on the former, and windows must be dropped while the event loop still runs.
- **Return `EventResult::Exit` once `running` is `None`.** `CloseRequested` tears the
  window down but does *not* end the loop; the exit comes from the next event arriving
  after the teardown. Returning `Wait` there leaves a live process with no window, which
  looks exactly like "the close button does nothing".

Also: `ViewportInfo::events` must be cleared once handed to egui, or a Close event repeats
every frame.

`tools/fork-rebase.sh` in the parent repo diffs that sha against the new tag so the port is
a small review, not a re-derivation. **Update `UPSTREAM_BASE` in the same commit as the
port.**

## Rebase runbook

```
git remote add egui-upstream https://github.com/emilk/egui   # once
GIT_LFS_SKIP_SMUDGE=1 git subtree pull --prefix=vendor/egui egui-upstream <new-tag> --squash
```

`GIT_LFS_SKIP_SMUDGE=1` is **required**: egui stores its demo/kittest snapshot PNGs in
git-LFS and those objects currently 404 from the upstream server. We never build those
crates, so checking the pointer files out instead is harmless — but without the variable the
subtree operation aborts partway.

### git-LFS breaks `git push` in a fresh clone

The ~230 inert pointer stubs that come with the subtree are enough to make git-LFS refuse
every push to the parent repo:

```
Git LFS upload failed:
  (missing) vendor/egui/crates/egui_demo_lib/tests/snapshots/.../dpi_2.00.png
hint: Your push was rejected due to missing or corrupt local objects.
```

git-LFS's `pre-push` hook scans the outgoing commits for pointer CONTENT and tries to upload
the objects behind them. It does not consult `.gitattributes` — commenting the `*.png
filter=lfs` rules out (which this fork does, so clones do not try to smudge) has no effect on
it. The objects do not exist anywhere: not locally, and not on upstream's LFS server.

This repository stores nothing in LFS, so the fix is to take LFS out of it:

```bash
git lfs uninstall --local          # removes .git/hooks/{pre-push,post-*} and filter.lfs.*
git config --local lfs.allowincompletepush true   # in case a later `git lfs install` returns
```

Both are LOCAL settings — git cannot commit them — so **a fresh clone has to run them
again**. Deleting the stub files instead would fix it once and for all, but they would come
back (or conflict) on every `git subtree pull`, so the two commands are the cheaper trade.

After pulling:

1. `cargo tree -d` from the parent repo — there must be no duplicated egui-repo crate. Two
   copies of egui produce errors like ``expected `egui::Context`, found `egui::Context` ``.
2. Re-check the divergence inventory above; drop anything upstream has adopted.
3. `tools/check-fork.ps1` (lints this workspace) **and** `python build.py --check` (lints the
   parent). Neither covers the other: `cargo clippy --workspace` in the parent does not deny
   warnings in an excluded path dependency.

### Rebase tripwires

Things that are safe today but would silently break the sub-pixel path:

- **Colour glyphs.** epaint 0.36.1 fills every glyph with `OpaqueColor::WHITE` — there is no
  COLR/CBDT path, so every atlas texel is coverage. The day upstream adds colour glyphs, the
  atlas needs a per-glyph "coverage vs colour" bit, because a colour glyph's RGB is not
  per-channel coverage.
- **`TextOptions` gaining fields upstream.** It is compared by value to decide atlas
  invalidation; a new field that we do not thread through means stale atlases.
- **`epaint::Vertex` layout changes.** The software rasterizer reproduces the glow shader
  against this exact layout.
- **`RowVisuals` / `glyph_vertex_range` changes** in `text_layout.rs`. The software text path
  consumes that mesh directly; it is the reason our glyph geometry is provably identical to
  the GPU backends rather than a reimplementation.

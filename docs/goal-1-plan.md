# Goal 1 Plan — Single-PDF Viewer with Page and Free Scroll

Researched 2026-07-29. All version numbers are as of that date.

**Status: M3 landed.** The viewer scrolls a whole document, and virtualization is real rather than
aspirational: a 400-page drawing set with two distinct page sizes holds two textures whether you
are at page 1, page 200, or page 400. The stack decision below is validated rather than assumed —
hayro parses and rasterizes correctly on Windows MSVC and Linux, 58 of 58 real-world PDFs on the
dev machine parsed without error, and renders have been inspected by eye rather than only
pixel-counted against a synthetic fixture. M4 (async raster pipeline) is next.

Two pieces of M4 arrived early at M3, because leaving them out would have shipped an obvious
defect rather than a known limitation:

- **Texture eviction.** Without it, scrolling 400 pages accumulates 400 textures, which breaks the
  bounded-memory exit criterion outright. Pages outside the visible range plus a two-page margin
  are dropped.
- **A per-frame render cap.** Rasterization is synchronous, so a viewport that suddenly shows many
  pages would otherwise block for as long as it takes to draw all of them. Two pages per frame
  keeps a frame bounded, and the remainder draw as placeholders and fill in next frame — which is
  also what makes the placeholder path real rather than theoretical.

Two things worth knowing before M3, both found by looking at output rather than by testing:

- **hayro renders on a transparent background by default.** A map filled its whole page so this
  was invisible; a text invoice exposed it immediately as black text on dark grey. Paper is white,
  so `porpoise-render` now has an explicit [`Background`] defaulting to opaque white. This
  affected the PNG output too, not just the window.
- **egui 0.34 renamed `App::update` to `App::ui`** and unified `TopBottomPanel`/`SidePanel` into
  `Panel`. Expect this class of churn at every egui bump; the changelog does not always mark it as
  breaking, and there are no migration guides.

## 1. The stack decision

### Rendering: `hayro` (pure Rust), not PDFium

This is the load-bearing call, and it flipped recently. Until late 2025 the honest answer was
"PDFium or nothing" — every pure-Rust renderer fell over on JPEG2000, JBIG2, and CCITT images,
which are common in scanned documents. That gap is now closed.

`hayro` (0.7.1, Apache-2.0 OR MIT) is a pure-Rust PDF interpreter and renderer by Laurenz
Stampfl, with from-scratch `forbid(unsafe_code)`, `no_std` decoders published as separate crates:

| Crate | Version | Covers |
|---|---|---|
| `hayro-jpeg2000` | 0.4.0 | JPX — 5/3 + 9/7 wavelets, JP2 + raw codestream, CMYK/ICC. No HTJ2K. |
| `hayro-jbig2` | 0.3.0 | Generic regions 0–3 + TPGDON, refinement, symbol dict, text, halftone, `/JBIG2Globals` |
| `hayro-ccitt` | 0.3.0 | G4, G3 1D, G3 2D; settings map 1:1 onto the PDF filter dictionary |

Supporting evidence for betting on it:

- **Typst 0.14 adopted it** for PDF-as-image ([announcement](https://typst.app/blog/2025/typst-0.14/)) — a real
  shipping product depends on its correctness.
- Tested against a **1400+ PDF corpus** scraped from the pdf.js regression suite, the PDFBox
  issue tracker, and the pdfa.org large-scale corpus.
- `hayro-interpret` exposes a **public `Device` trait** — we can receive drawing commands and
  drive our own renderer, and build a text/selection model from the same interpretation pass.
  This matters a lot for Goal 2 (see §5).
- Permissive license, no C toolchain, no binary to ship.
- Every other new pure-Rust PDF project (`stet`, `pdf_oxide`, `zpdf`) now consumes hayro's
  codec crates rather than reimplementing them. The ecosystem has converged here.

### Why not PDFium, given it is the industry default

Our stated goal is maximum efficiency **and security**, and PDFium is the wrong side of that trade:

- **128 CVEs all-time**, with **14 published between 2026-03-29 and 2026-07-27** — nearly all
  CVSS 8.8 use-after-free or heap buffer overflow reachable from a crafted PDF.
- PDFium's threat model *assumes Chrome's renderer sandbox*. Embedding it in-process in a
  desktop app means memory-unsafe parsing of untrusted input with no sandbox. We would have to
  build process isolation ourselves just to reach parity with what pure Rust gives us free.
- **No incremental save.** `pdfium-render`'s `save_to_writer` hardcodes `let flags = 0;` behind a
  TODO open since 2022-05-25. That blocks a core editor requirement.
- Distribution depends on two single-maintainer binary repackaging repos
  (`bblanchon/pdfium-binaries`, `paulocoutinhox/pdfium-lib`) — 27 of 30 and 8 of 8 recent
  commits respectively by one person each.

**But PDFium earns a place in the test harness.** The research turned up no published
head-to-head accuracy comparison of hayro vs PDFium anywhere — hayro's own `hayro-bench` README
says the comparison harness is unbuilt, and its test suite uses self-generated baselines, making
it a *regression* suite rather than a correctness oracle. So we build the oracle ourselves:
`pdfium-render` as a **dev-only dependency** that pixel-diffs hayro output across a corpus. It
never enters the shipped binary. This is cheap, it directly de-risks our single biggest
dependency, and it produces information that does not currently exist publicly.

### Document model for later editing: `lopdf`

Not needed for Goal 1, but it determines the shape of `porpoise-doc`, so decide now.
`lopdf` 0.44.0 (MIT) has a real `IncrementalDocument` type — `create_from`,
`opt_clone_object_to_new_document`, `Document::new_from_prev` — for appending updates while
preserving the original bytes. That is the correct way to save an edited PDF, and it is the only
permissive crate that supports it (`mupdf` does too, but it is **AGPL-3.0** — disqualifying).
`lopdf` also has the healthiest bus factor of anything surveyed: 102 contributors, and its
top recent committer accounts for only 11 of 84 commits.

### Shell: `eframe`/`egui` 0.35

| | Why it wins for us |
|---|---|
| wgpu | On **wgpu 29** (current). `egui_wgpu::CallbackTrait` hands you `&mut RenderPass<'static>` inside normal layout — the most ergonomic custom-GPU-pass story of any Rust toolkit. |
| Accessibility | AccessKit is **non-optional** since 0.34 and now load-bearing internally (0.35's inspection protocol reads the AccessKit tree). A world-class editor needs screen-reader support. |
| Text selection | Cross-widget text selection already exists (`style.interaction.multi_widget_text_select`). Bare-bones, but a starting point for Goal 2. |
| Funding | Sponsored by Rerun; emilk is Rerun's co-founder and the Rerun Viewer is built on egui. Verifiable, unlike most alternatives. |
| Cadence | 12 releases in 12 months. |

Rejected, with reasons:

- **iced** — one release in 12 months after a 14.5-month gap, **zero** patch releases on 0.14
  (bugfixes live only on master), still on wgpu 27 (two majors behind), and
  [accessibility open since 2020-10-05](https://github.com/iced-rs/iced/issues/552). Funding
  unclear — the README still credits a team Kraken sunset in 2023.
- **gpui** — no wgpu on Windows at all (Direct3D 11 + DirectWrite), and the crates.io publish is
  9 months stale in a way that does not resolve: the README tells you to depend on `gpui` +
  `gpui_platform`, and `gpui_platform` is not published. Three community forks exist because of
  this. *However — see §5, there is directly relevant prior art here worth reading.*
- **xilem/masonry** — has the best long-term ingredients (parley, deepest AccessKit wiring,
  VirtualScroll with a11y integration) but no wgpu interop
  ([#395](https://github.com/linebender/xilem/issues/395) stalled since 2024), alpha-quality by
  its own description, and still no changelog 9 months after promising one. Revisit in 2027.
- **Slint** — tri-licensed (closed-source desktop is free with attribution), but the wgpu feature
  flag renames every release, text-input accessibility has been
  [open since 2023-06-15](https://github.com/slint-ui/slint/issues/2895), and there is an open
  a11y performance issue on large list views — a direct threat to a virtualized canvas.
- **Tauri** — no native wgpu, and canvas content is opaque to assistive tech. For an app whose
  primary surface *is* the canvas, the DOM accessibility advantage does not apply.

The immediate-mode ceiling is a genuine risk once we reach editor-grade UI complexity. §3
mitigates it structurally: all viewer logic lives in a GUI-agnostic crate, so a shell swap costs
one crate, not a rewrite.

### Pins

MSRV **1.92**, edition **2024** (hayro's requirement).

Corrected 2026-07-29: an earlier draft of this document claimed hayro pins `vello_cpu` to a git
rev of linebender/vello, and that this would block publishing our crates. That is wrong.
Published `hayro` 0.7.1 depends on `vello_cpu ^0.0.8` from crates.io — and it must, because
crates.io forbids git dependencies in published crates. The git pin exists in hayro's own
development workspace, not in what we consume. `deny.toml` therefore sets
`sources.unknown-git = "deny"`, so a git dependency cannot enter our graph unnoticed.

Also note the resolved codec versions lag the latest published ones: hayro 0.7.1 pulls
`hayro-jpeg2000` **0.3.5**, not the 0.4.0 in the table above. Worth re-checking when hayro
next releases, since 0.4.0 is where the JPX work landed.

## 2. What "Goal 1" actually requires

Rendering one page is a weekend. The engineering that separates a world-class viewer from a toy
is all in the scroll pipeline:

- **Heterogeneous page geometry.** Real PDFs mix page sizes and `/Rotate` values. Scroll position
  cannot be `page_index * page_height`. We build a cumulative offset index up front from each
  page's MediaBox/CropBox — cheap, no rasterization needed.
- **Virtualization.** Only rasterize the visible set plus a prefetch margin. Memory must be
  bounded by viewport size, not document length.
- **Off-main-thread rasterization.** hayro's own README says performance "has not been a focus at
  all so far." Rasterization must never touch the UI thread.
- **Cache keyed by (page, zoom bucket)** with an LRU byte budget. Quantize zoom into buckets so
  a pinch-zoom gesture does not thrash the cache.
- **Placeholders.** Correct-aspect-ratio blanks render instantly so scroll never blocks on I/O.
- **Cancellation.** Fast scrolling must abandon in-flight raster jobs for pages that left the
  viewport.

### Security, restated honestly

Pure Rust converts *remote code execution* into *denial of service*. It does not eliminate DoS,
and hayro has open panic issues today (#717, #646, #373 inside `vello_cpu`; #404 on large
xStep/yStep). So the untrusted-input boundary needed real handling from M1, not bolted on later.

**Landed at M1:**

- Rasterization wrapped in `catch_unwind`, so a panic ruins one page rather than the process.
  Note hayro is `!UnwindSafe` — it holds interior-mutable caches — so `AssertUnwindSafe` is
  load-bearing, not decorative.
- `RenderLimits` bounds the allocation *before* the backend is invoked, with both a per-axis cap
  and a **total-pixel cap**. The second one matters more than it looks: 65535x65535 is within
  hayro's per-axis `u16` viewport limit and still asks for roughly 17 GB. Per-axis checks alone
  are not a limit.
- `render_with_timeout` bounds wall-clock time, because a malformed document can make the
  interpreter loop rather than panic, and no amount of memory safety helps with that.

**Deliberately not done, and why:**

- *Max decompressed image bytes* and *recursion depth caps* were in the original plan. They are
  not implementable through hayro's public API — it exposes no hooks for either. The timeout and
  the area cap are what we actually have, and they cover the realistic cases. Revisit if we
  implement `Device` ourselves, which would put us inside the interpretation loop.
- *Real cancellation.* Rust cannot cancel a thread, so a timeout abandons the worker rather than
  killing it. An infinite loop occupies a core until the process exits. Acceptable for a one-shot
  CLI render; **not** acceptable for the viewer, which needs a bounded worker pool at M4 so
  timeouts cannot accumulate, and process isolation after that.

## 3. Project structure

A workspace, so the security-critical and GUI-agnostic parts stay independently testable:

```
Porpoise_PDF/
├─ Cargo.toml                # workspace root, shared lints + dep versions
├─ deny.toml                 # cargo-deny: license allowlist
├─ GOALS.md
├─ docs/
│  └─ goal-1-plan.md
└─ crates/
   ├─ porpoise-doc/          # open a PDF; page count, geometry, rotation, metadata
   ├─ porpoise-render/       # Renderer trait + hayro backend; page + scale -> RGBA pixmap
   ├─ porpoise-view/         # GUI-agnostic: layout, scroll, zoom, virtualization, cache policy
   ├─ porpoise-app/          # eframe shell: the binary, input handling, texture upload, chrome
   └─ porpoise-testkit/      # dev-only: corpus runner, pixel-diff, PDFium oracle, fuzz targets
```

Why these seams specifically:

- **`porpoise-doc` / `porpoise-render` split** — the editor needs the document model without the
  renderer. `porpoise-doc` is a thin facade over hayro for Goal 1; it is the seam where `lopdf`
  joins for incremental save later, without the shell knowing.
- **`porpoise-render` as a trait, not a direct call** — lets the PDFium oracle be a second
  implementation of the same trait in tests, and lets a future GPU backend (§5) slot in.
- **`porpoise-view` is the important one.** Scroll geometry, visible-set computation, zoom
  bucketing, and cache eviction are pure logic over numbers. Keeping them out of the GUI crate
  means they are unit-testable with zero windowing, and it caps the cost of a shell migration.
- **`porpoise-testkit` separate** so corpus PDFs and the PDFium dev-dependency cannot leak into
  the shipped binary.

`porpoise-view` and `porpoise-testkit` will be nearly empty at M1. Creating them up front is
free and prevents the monolith.

## 4. Milestones

| | Deliverable | Proves |
|---|---|---|
| **M0** ✅ | Workspace skeleton. CI: fmt + clippy + test on Windows and Linux, MSRV floor job, `cargo-deny`, and a job asserting no C PDF/codec library reaches the shipped binary. Page geometry, the `Renderer` trait seam, and `ScrollLayout` are real and tested. | The AGPL/GPL traps (§6) are mechanically excluded, not remembered — and the pure-Rust stack demonstrably rasterizes on Windows. |
| **M1** ✅ | Headless CLI: `porpoise info` and `porpoise render --page N --dpi D -o out.png`. `RenderLimits` (per-axis **and** total-pixel caps), `render_with_timeout`, `catch_unwind`, PNG encoding. | hayro works on real files. Fully testable with no GUI. |
| **M2** ✅ | eframe window (wgpu backend), page 1 fit-to-width, opened from a CLI path. Plus a hidden `--screenshot` flag so the window can be verified headlessly. No file dialog yet. | End-to-end pixels on screen. |
| **M3** ✅ | Continuous scroll across all pages at one shared zoom, drawing only the visible set, with placeholders and texture eviction. Rasterization still synchronous, capped at 2 pages per frame. `--start-page` opens deep in a document. | Scroll geometry is right. Verified on a 400-page two-page-size document at pages 1, 200 and 400, holding **2 textures** throughout. |
| **M4** | Async raster pipeline: worker pool, LRU byte budget, prefetch margin, zoom buckets, job cancellation. | Smoothness. This is the milestone that makes it feel good. |
| **M5** | Mode switch — paged (snap) vs free scroll. Keyboard nav (PgUp/PgDn, Home/End, arrows), ctrl+wheel zoom, fit-width/fit-page. | **Goal 1 feature-complete.** |
| **M6** | Hardening: malformed-PDF corpus run, `cargo-fuzz` targets on the parse path, PDFium differential pixel-diff. | It survives hostile input, and we have data on hayro's accuracy. |

### Exit criteria

Goal 1 is done when, on a mid-range laptop:

- Sustained 60 fps free-scrolling through a 400+ page document.
- Resident memory bounded and roughly flat regardless of document length (target < 500 MB).
- Time from launch to first page painted on a 100 MB PDF under 1 second.
- Zero hangs, zero process crashes across the malformed corpus — broken pages render as error
  placeholders.

**Benchmark document.** The standing M4 performance target should be a document with roughly
these properties: **400 pages, two distinct page sizes** (e.g. 792x612 and 1224x792), and
~321,000 pt of total scroll height. That combination exercises heterogeneous geometry,
long-document virtualization, and the memory budget simultaneously — a uniform-page document
will not.

A file matching this profile has been identified locally. Copy it to a stable path outside the
repository, and keep the path in a git-ignored local config rather than committing either the
file or its name: benchmark corpora are generally not ours to redistribute.

## 5. What we build ourselves

Nothing at first. Every piece of Goal 1 is available as a maintained permissive crate, and
writing our own renderer now would be the single fastest way to never ship. What we *do* own:

- **The `porpoise-*` crates** — the document facade, the scroll/virtualization engine, and the
  cache policy. This is the actual product surface, and none of it exists off the shelf.
- **The differential test harness** — genuinely novel, per §1.
- **A GPU render backend, Phase 3+, via hayro's `Device` trait.** hayro rasterizes on CPU
  (`vello_cpu`), so our Goal 1 pipeline is `hayro → CPU pixmap → GPU texture upload`. That is
  fine, and it means we do not need egui's custom wgpu pass for Goal 1 at all. Later, we can
  implement `Device` ourselves and rasterize on GPU. Note the actual signature is
  **per-glyph** — `draw_glyph(&mut self, glyph, transform, glyph_transform, paint, draw_mode)` —
  there is no `GlyphRun` batching, so per-call overhead is a real question to measure before
  committing.
- **The text layer, Goal 2.** hayro has **no text-extraction API**; implementing `Device` and
  collecting positioned glyphs is the sanctioned workaround. Since each glyph arrives with its
  full transform, the same pass yields selection, search, and the AccessKit text tree. Worth
  knowing now because it validates the `porpoise-render` trait boundary.

### Prior art to read before writing M3–M4

[`gpui-pdf`](https://github.com/packetThrower/zorite/tree/main/crates/gpui-pdf) describes itself
as "page-virtualized PDF viewing built on the pure-Rust hayro rasterizer — zoom, navigation,
full-text search, and bounded memory, with no native dependencies." That is our exact
architecture on a different shell. Read it before designing the cache and virtualization.

### The bus-factor problem, and the right response

hayro is **359 of 375 recent commits by one person**. `pdfium-render` is 59 of 72 by one person.
This is unavoidable — it is true of essentially every option in this space.

The wrong response is to fork or rewrite. The right response is threefold: pin exact versions and
vendor the source so an abandoned upstream cannot break our build; keep `porpoise-render` a trait
so a backend swap is bounded work; and **contribute upstream to hayro** rather than around it.
Becoming a credible second contributor to our most critical dependency is a strategic goal, not a
side effect — and it is far cheaper than owning a PDF renderer.

## 6. License traps to encode in `deny.toml`

Found while researching; all are easy mistakes to make:

- **`mupdf` / mupdf-rs is AGPL-3.0.** Upstream MuPDF is Artifex AGPL plus paid commercial.
- **The crate literally named `pdfium` (`newinnovations/pdfium-rs`) is GPL-3.0**, with 24.5k
  downloads. `pdfium-render` is the permissive binding. PDFium itself being BSD-3 does not save
  you from a GPL wrapper.
- **`pdf-render` on crates.io is not pdf-rs's renderer.** It is published under `pdfluent.com`,
  whose upstream LICENSE reads "proprietary and confidential… NOT open-source software" and whose
  EULA forbids linking, despite permissive crates.io metadata. Treat as unsafe to depend on. There
  is a matching set of crates (`pdf-syntax`, `pdf-interpret`, `pdfluent-jbig2`, …) with
  descriptions byte-identical to hayro's, provenance unverified.
- **`hayro` carries NOTICE obligations** — Apache-2.0 code adapted from PDFBox, pdf.js, and the
  `png` crate. Permissive, but `NOTICE.md` must be propagated.
- **AccessKit's repo reports BSD-3-Clause** because of a `LICENSE.chromium` file, though the
  crates are MIT OR Apache-2.0. Worth a lawyer's glance if we ever ship commercially.

## 7. Open decisions

1. ~~**Our license.**~~ **Decided 2026-07-29: `MIT OR Apache-2.0`**, copyright Christian Bailey,
   open source. Apache-2.0 supplies the explicit patent grant that MIT lacks — worth having when
   the format's image codecs carry a long patent history — while MIT keeps the code usable by
   GPLv2 projects that Apache-2.0 alone excludes. Matches `hayro`. One consequence to track:
   Apache-2.0 section 4(d) obliges us to propagate hayro's `NOTICE.md` (PDFBox, pdf.js, `png`)
   once we distribute binaries, so a generated `THIRD-PARTY-NOTICES` is a prerequisite for the
   first release, not for M1.
2. **Confirm the egui bet.** It is the right call for Goal 1 and defensible for years, but if a
   distinctly non-egui-looking UI is a hard product requirement, that is worth knowing before M2.
3. **Corpus sourcing for M6.** pdf.js and PDFBox regression suites are the obvious starting
   points; check their licenses before vendoring.

## 8. Known unknowns

Flagged rather than glossed, because each is a real risk:

- **No independent hayro-vs-PDFium correctness comparison exists.** Its breadth is inferred from
  corpus size and Typst adoption. M6 is how we replace inference with data.
- **hayro's documented gaps:** knockout groups, non-embedded CID fonts. Type 3 fonts render but
  [lack character-code access](https://github.com/LaurenzV/hayro/issues/1331) — that is a Goal 2
  text-extraction problem, not a Goal 1 one.
- **Mesh shadings (PDF types 4–7)** — could not confirm whether hayro implements them.
- **12- and 16-bit JPEG** — `zune-jpeg` hard-rejects any precision other than 8 bits, and no
  pure-Rust alternative was found. Rare in the wild, but a real hole.
- **`vello_cpu` self-describes contradictorily** — its own README says "ready for production use
  cases" while `sparse_strips/README.md` says the family is "not production-ready." We inherit
  this transitively through hayro either way.

# Goal 1 Plan — Single-PDF Viewer with Page and Free Scroll

Researched 2026-07-29. All version numbers are as of that date.

**Status: Goal 1 complete.** All four exit criteria are measured rather than asserted. M6's PDFium
differential oracle was dropped by decision rather than left undone — building in pure Rust is the
objective, so PDFium is not the standard being measured against. See sections 1 and 6a.

Previously: **M5 landed — Goal 1 is feature-complete.** You can open a PDF, scroll it freely or page by
page, navigate by keyboard, and zoom by wheel, pinch, key, or fit mode. Frame times under a
synthetic 40-pages-per-second scroll are measured, not assumed: see the exit criteria. M6
(hardening) is what remains.

Rasterization is off the UI thread. The frame loop polls for finished pages,
requests missing ones, and paints whatever the cache holds — it never waits. On the 400-page
drawing set at page 200 that is 6 cached pages and **15.7 MB**, against a 192 MB budget and a
500 MB target for the whole document.

The stack decision below is validated rather than assumed — hayro parses and rasterizes correctly
on Windows MSVC and Linux, 58 of 58 real-world PDFs on the dev machine parsed without error, and
renders have been inspected by eye rather than only pixel-counted against a synthetic fixture.
M5 (paged scrolling, keyboard navigation, explicit zoom) is next.

Three details do most of the work in making it feel continuous rather than merely asynchronous:

- **Zoom bucketing.** Renders key to a quantized zoom rung on a geometric ladder — eight rungs per
  octave, so relative error is constant across the whole zoom range. Without this, a window drag
  invalidates every texture on every pixel of movement.
- **Stale-resolution fallback.** While the correct rung renders, the nearest cached rung is drawn
  scaled. Slightly soft beats a grey flash, and it is what makes a resize look continuous.
- **Prefetch.** Pages just outside the viewport are requested *after* all visible ones, so
  ordinary scrolling usually finds the next page already rendered.

Two subtleties worth recording, both found by tests rather than by reasoning:

- **`ZoomBucket::enclosing` must be idempotent.** `log2` of a rung's own scale lands a hair below
  the integer, so a naive `ceil` climbs one rung every layout pass and the zoom creeps upward
  forever. A small slack term before rounding fixes it, at the cost of a texture up to 0.1%
  smaller than its display size.
- **Positive infinity is not a degenerate zoom, it is an absurd one.** Grouping it with `NaN`
  snapped it to the *floor*, which would have rendered a hugely magnified page at minimum
  resolution. It clamps to the ceiling instead.

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
  This matters a lot for the text layer (see §5), which is Goal 4.
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

**PDFium is not a yardstick either.** An earlier version of this document proposed keeping
`pdfium-render` as a dev-only dependency to pixel-diff hayro's output, on the reasoning that no
published head-to-head accuracy comparison exists and we should produce one.

**Decided 2026-07-29 (Christian): no.** That framing mistook the goal. Building this in pure Rust
is the objective, not a route to parity with an existing C++ engine — so PDFium's output is not the
standard being aimed at, and a comparison against it measures the wrong thing. The oracle harness
was written, then removed rather than left as scaffolding that would never run.

What replaces it: validation against the *format* and against real documents — visual inspection of
real renders, deterministic regression via `pixel_diff`, and the malformed-input sweep in
section 6a. The consequence to hold onto is that hayro's absolute rendering fidelity is accepted on
the strength of Typst shipping it and its 1400-document corpus, rather than independently measured
here. That is a deliberate premise, not an outstanding task.

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
| Text selection | Cross-widget text selection already exists (`style.interaction.multi_widget_text_select`). Bare-bones, but a starting point for the text layer in Goal 4. |
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
  killing it. **Improved at M4 but not solved:** `RenderPool`'s workers call `render_with_timeout`
  rather than rendering directly, so a hung page abandons one anonymous thread while the worker
  itself returns to the queue. That means the pool cannot be starved — there is a test for exactly
  this — but a hung render still leaks a thread until the process exits. Process isolation remains
  the real fix.

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
   └─ porpoise-testkit/      # dev-only: PDF fixtures, pixel-diff, malformed-input mutator
```

Why these seams specifically:

- **`porpoise-doc` / `porpoise-render` split** — the editor needs the document model without the
  renderer. `porpoise-doc` is a thin facade over hayro for Goal 1; it is the seam where `lopdf`
  joins for incremental save later, without the shell knowing.
- **`porpoise-render` as a trait, not a direct call** — lets tests drive the pipeline with a stub
  backend, and lets a future GPU backend (§5) slot in. Originally justified by wanting a second
  *engine* behind the same trait; that reason is gone with the oracle, but the stub renderer the
  pool tests depend on earns the seam on its own.
- **`porpoise-view` is the important one.** Scroll geometry, visible-set computation, zoom
  bucketing, and cache eviction are pure logic over numbers. Keeping them out of the GUI crate
  means they are unit-testable with zero windowing, and it caps the cost of a shell migration.
- **`porpoise-testkit` separate** so fixtures and test-only dependencies cannot leak into the
  shipped binary. This was asserted by a CI job until CI was removed; the `cargo tree` check that
  did it is now in the README's *Checks* section, and nothing runs it automatically.

`porpoise-view` and `porpoise-testkit` will be nearly empty at M1. Creating them up front is
free and prevents the monolith.

## 4. Milestones

| | Deliverable | Proves |
|---|---|---|
| **M0** ✅ | Workspace skeleton. CI: fmt + clippy + test on Windows and Linux, MSRV floor job, `cargo-deny`, and a job asserting no C PDF/codec library reaches the shipped binary. Page geometry, the `Renderer` trait seam, and `ScrollLayout` are real and tested. | The AGPL/GPL traps (§6) are mechanically excluded, not remembered — and the pure-Rust stack demonstrably rasterizes on Windows. |
| **M1** ✅ | Headless CLI: `porpoise info` and `porpoise render --page N --dpi D -o out.png`. `RenderLimits` (per-axis **and** total-pixel caps), `render_with_timeout`, `catch_unwind`, PNG encoding. | hayro works on real files. Fully testable with no GUI. |
| **M2** ✅ | eframe window (wgpu backend), page 1 fit-to-width, opened from a CLI path. Plus a hidden `--screenshot` flag so the window can be verified headlessly. No file dialog yet. | End-to-end pixels on screen. |
| **M3** ✅ | Continuous scroll across all pages at one shared zoom, drawing only the visible set, with placeholders and texture eviction. Rasterization still synchronous, capped at 2 pages per frame. `--start-page` opens deep in a document. | Scroll geometry is right. Verified on a 400-page two-page-size document at pages 1, 200 and 400, holding **2 textures** throughout. |
| **M4** ✅ | Async raster pipeline: `RenderPool` worker threads, byte-budgeted LRU `PageCache` keyed by page and zoom rung, prefetch margin, `ZoomBucket` quantization, queued-job cancellation, and stale-resolution fallback. | Smoothness. The UI thread never waits for a render. |
| **M5** ✅ | Paged vs free scroll, keyboard navigation, ctrl+wheel and pinch zoom, fit-width/fit-page, a toolbar, and `--scroll-benchmark` for frame-time measurement. | **Goal 1 feature-complete.** |
| **M6** ✅ | Parse-path panic isolation, a 4,000-case deterministic mutation harness, exhaustive truncation coverage, allocation-bomb rejection. PDFium oracle dropped by decision, `cargo-fuzz` deferred; both explained in section 6a. | It survives hostile input. |

### Exit criteria

Goal 1 is done when, on a mid-range laptop:

- ✅ **Sustained 60 fps free-scrolling through a 400+ page document.** Measured with
  `--scroll-benchmark 600` over the whole 400-page document — a synthetic scroll of roughly 40
  pages per second, far faster than anyone scrolls by hand. Frame interval **p50 16.66 ms
  (60.0 fps), p95 17.9 ms, p99 20–23 ms**. Our own cost is **≤ 2.4 ms** of the 16.67 ms budget
  (logic including GPU upload ≤ 2.1 ms, ui ≤ 0.4 ms), so roughly 86% of the frame is headroom.
  See the caveat below.
- ✅ **Resident memory bounded and roughly flat regardless of document length.** 15.7 MB of page
  textures at page 200 of 400, against a 192 MB budget and the 500 MB target.
- ✅ **Time from launch to first page painted on a 100 MB PDF under 1 second.** Measured with
  `--time-to-first-page` on a **132 MB**, 400-page document: **552–630 ms** across three runs. The
  clock starts before argument parsing, so it includes reading the file and creating the window.
- ✅ **Zero hangs, zero process crashes across the malformed corpus.** 4,000 deterministic
  mutations of a valid PDF — truncation, bit flips, zeroed runs, junk, damaged header, damaged
  `startxref`, duplicated runs — plus every one of the 465 possible truncation lengths
  exhaustively. Every input either opened or returned an error; **zero panics escaped**, and
  notably zero were even contained by the new parser `catch_unwind`. Mean time to reject damaged
  input is 6.7 µs, so a slow-parse denial of service is not available either.

**Open performance item: an unattributed frame-time outlier.** Each 600-frame run shows a single
frame around 150–160 ms. It is not our code: instrumenting `logic` and `ui` separately accounts for
at most 2.4 ms of that frame. It is not startup either, since the benchmark discards 60 warmup
frames and reports their worst separately (30–75 ms). The obvious hypothesis — that per-frame
texture allocation churn stalls the driver — was **tested and disproved**: widening the retain
window from 3 to 8 pages left the tail unchanged across repeated runs. So the cause lies below our
layer, in eframe, wgpu, the driver, or the compositor, and is not yet explained. Worth chasing
before calling the viewer world-class, but it does not block Goal 1: 99% of frames are within
budget under a scroll rate no human generates.

**Not perceptible in real use (Christian, 2026-07-29).** Scrolling the 400-page document by hand
felt responsive, with no hitch reported. Combined with the benchmark's ~40 pages per second, that
suggests the outlier is provoked by the synthetic scroll rate rather than by ordinary interaction.
Still worth explaining eventually — an unexplained stall is a latent risk — but it should be
prioritised as a curiosity, not as a user-facing defect.

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
- **The malformed-input mutation harness** — deterministic, seeded, and reproducible from its
  seed, which off-the-shelf fuzzing does not give us without a corpus and a separate build.
- **A GPU render backend, Phase 3+, via hayro's `Device` trait.** hayro rasterizes on CPU
  (`vello_cpu`), so our Goal 1 pipeline is `hayro → CPU pixmap → GPU texture upload`. That is
  fine, and it means we do not need egui's custom wgpu pass for Goal 1 at all. Later, we can
  implement `Device` ourselves and rasterize on GPU. Note the actual signature is
  **per-glyph** — `draw_glyph(&mut self, glyph, transform, glyph_transform, paint, draw_mode)` —
  there is no `GlyphRun` batching, so per-call overhead is a real question to measure before
  committing.
- **The text layer, Goal 4.** hayro has **no text-extraction API**; implementing `Device` and
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

## 6a. Hardening: what M6 did and did not do

**Parse-path panic isolation was a real gap.** Rasterization had been wrapped in `catch_unwind`
since M1, but `Document::from_bytes` had not — so a panic in hayro's *parser* would have taken down
the whole application rather than failing to open one file. Now contained, with a distinct
`DocumentError::ParserPanicked` variant, because a panic is a bug to triage rather than an ordinary
rejection.

**hayro's parser proved more robust than expected.** Across 4,000 mutations, *zero* panics were
contained — the isolation never fired. 3,555 of those mutations still opened successfully, which
says the parser recovers from a great deal of damage. The `catch_unwind` stays regardless: hayro has
open panic issues on record, and the cost of the guard is nil.

**`cargo-fuzz` was deliberately not added.** It needs a nightly toolchain and libFuzzer via clang,
is awkward on Windows MSVC, and cannot run in our stable CI — so committing a fuzz target would
mean shipping scaffolding that is never exercised and quietly rots. The mutation harness covers the
same surface, runs on every push on both platforms, and reproduces any failure from its seed. What
coverage a real fuzzer would add over that is *structure-aware* mutation guided by coverage
feedback, which is genuinely better; the honest position is that it is worth doing when there is a
nightly job to run it in, not that it is done.

**The PDFium oracle was built and then removed.** It compiled, and skipped cleanly when no PDFium
library was present. It was deleted on the decision recorded in section 1: pure Rust is the goal,
so measuring against PDFium measures the wrong thing, and keeping a binding to the engine we
rejected would have been scaffolding that never runs — the same objection raised against
`cargo-fuzz` immediately above.

The `deny.toml` bans on C PDF and codec libraries stay, and matter *more* under this framing, not
less: they are the mechanical enforcement of the premise. The `no-native-deps` job that backed them
up went when CI did — its check survives as a command in the README, which is weaker, because it now
depends on somebody running it. `pixel_diff`
also stays — it is what makes hayro's own output testable for determinism and regression.

## 6b. Post-Goal-1 code audit (2026-07-29)

Goal 1 was reviewed for code quality, module boundaries, test coverage, and documentation accuracy
once it was complete. All four automated gates were already green — 93 tests, clippy with
`-D warnings`, rustdoc with `-D warnings`, and `cargo-deny` — so the findings were the things a gate
does not catch.

**Documentation had drifted from the code in ten places.** Every one described the PDFium
differential oracle as existing or forthcoming: the README's state line and crate table, a whole
paragraph of `porpoise-render`'s crate docs, `porpoise-testkit`'s crate docs and package
description, the `no-native-deps` CI comment, and four spots in this document's own §3 and §5. All
corrected. This is worth naming as a category rather than a list: when a decision reverses, the
prose that justified the old decision is scattered much wider than the code that implemented it,
and only the code gets deleted.

**Test coverage was inverted against risk.** `porpoise-view` — 465 lines of pure arithmetic — had
649 lines of tests. `porpoise-app` — 1,168 lines holding every piece of mutable state in the
program — had none. That is backwards: the arithmetic is the part least likely to be silently wrong,
because it is the part with no hidden state. Fixed by splitting the dev instrumentation into
`devtools.rs`, extracting the CLI's pure decisions (`page_index`, `resolve_scale`, `log_level`) from
the I/O around them, and testing all of it — 34 tests where there were zero.

That immediately paid for itself: `to_color_image`, the guard whose entire job is to stop a bad
buffer from panicking on the UI thread, **accepted a zero-dimension page**. A zero width satisfies
the length check trivially — zero bytes is exactly what `0 * h * 4` asks for — and the image then
reaches `load_texture`, where wgpu validates dimensions. Not reachable through `HayroRenderer`,
which refuses a sub-pixel page first, so this was a latent hole rather than a live bug. It was found
by writing the obvious test for a function that had never had one.

**Three defects in shipped behaviour:**

1. **A timed-out page was blacklisted forever.** `failures` was keyed by rasterization and only
   cleared on a zoom change, so one timeout meant a permanent error tile at that zoom — and the
   field's own comment claimed the opposite. Timeouts now carry a bounded retry budget
   (`MAX_RENDER_RETRIES`) while deterministic failures — panic, refused size, bad index — still get
   none, because retrying those only burns a worker to reach the same answer.
2. **Every `tracing` call went nowhere.** `porpoise-render` has three `warn!` sites and no
   subscriber was ever installed, so "the renderer panicked on this page" — the single diagnostic
   this project's premise most depends on — was discarded. `porpoise-app` now installs one on
   stderr, honouring a bare `RUST_LOG` level.
3. **Dead declarations and dead code.** `rayon`, `parking_lot`, `insta` and `rfd` were declared in
   `[workspace.dependencies]` and used by nothing; we wrote our own pool, used std locks, and never
   wrote a snapshot test. They cost no build time — an unused workspace declaration pulls nothing —
   but they read as "we use these." Removed. `lopdf` stays, as the one forward reference with a
   stated reason. `PixelDiff::fraction_differing` was never called and is gone; `pixel_diff`'s
   `tolerance` parameter was only ever passed zero and now has a test that exercises it.

**Module boundaries: crate layering was right, file sizes were not.** `doc ← render ← app` and
`doc ← view ← app` with no cycles, and `porpoise-view` depending on exactly one crate, is the part
that would have been expensive to get wrong. But `viewer.rs` was 901 lines with ~200 of them
measurement apparatus interleaved with the thing being measured, and `porpoise-view/src/lib.rs` held
three unrelated concerns in 570 lines while the *smaller* concerns had each been given their own
module. `porpoise-view` is now five focused modules behind the same public API; `porpoise-app` is
three.

**Still open after the audit:** the ~150 ms frame outlier (§6a) reproduces unchanged and remains
unexplained, though `ui` time at its worst is 0.24 ms, so it is still not our code. There are no
doctests anywhere in the workspace — the prose is thorough but nothing compiles it, so it can drift
the same way §3 and §5 just did.

## 6c. Regression: paged scrolling could not reach the last page (2026-07-30)

Reported by a person using the program, not by a test. In paged mode the counter stuck one short of
the end — *page 9 of 10* — and `PageDown` did nothing, while the last sheet sat fully visible on
screen.

### The arithmetic

Scrolling stops at `content_height - viewport_height`. The last page's top is at
`content_height - last_page_height`. So the moment **the window is taller than a page**, the last
page's top is *past the end of the scroll range*: you can see all of it, and you can never put its
top edge at the top of the window.

`current_page` was "the topmost page overlapping the viewport". At the bottom of the document the
previous page still overlaps by a sliver, so it kept the view. Measured on `ROLT14_GDOT-U_6.pdf`:

| Zoom | Viewport | Scroll stops at | Last page's top | Reported |
|---|---|---|---|---|
| fit-width | 865 pt | 7162.6 | 7236.0 | page 9 ❌ |
| 1.0 | 724 pt | 7304.0 | 7236.0 | page 10 ✅ |

Fit-width is the default, and these are 792 pt sheets in a 1024x768 window — so it was broken by
default on every landscape drawing set and correct as soon as you zoomed in. That is why it read as
a regression rather than as a missing feature.

### What introduced it

`1c43f6e`, the fix that stopped screen pixels being used as PDF points. Before it, the viewport's
height was taken as 724 *points* at every zoom, and 724 < 792 — so the last page happened to be
reachable. **The units fix was correct and exposed a latent bug**: the third time in this project
that something unreachable turned out to be mishandled the moment it became reachable.

### The fix, and two definitions that do not work

`current_page` is now **the first visible page showing at least half of as much of itself as it
possibly could** — half of `min(page height, viewport height)` — falling back to the topmost visible
page. `ScrollLayout::current_page` owns it, and `step_page` reads the same function, which is what
makes forward and backward stepping agree with the counter.

- **The viewport's centre** was the original rule. A window taller than a page has its centre in the
  *next* page, so `GoToPage(3)` reported 4.
- **Most visible page** fails with pages of mixed heights: a short page followed by a tall one has
  the tall one occupying more of the window right after you navigate to the short one.
- **Half of what it could show** survives both, and not by luck: navigating puts a page's top at the
  window's top, which shows the most of it geometry allows — twice the threshold, at any zoom, page
  size or window size. So the round trip holds *by construction*.

One honest limit: when the window is tall enough to show several pages, the pages near the end cannot
be scrolled to at all, so `GoToPage(N)` then asking cannot return N for them. That is the scroll
clamp, not the rule, and no definition of "current page" can change it.

### Why no test caught it

Every `porpoise-view` test used a viewport **half a page tall** — the opposite of the condition. The
regression needed a window *taller* than a page, and nothing exercised that. There are now nine tests
that do, three of which were confirmed to fail against the old rule before the fix was kept.

## 6d. Paged mode was a key binding, not a view (2026-07-30)

Reported by the same person, straight after §6c: *"Even when paged is selected, it still scrolls like
a free scroll."* Correct, and the diagnosis is embarrassing in a useful way — **paged mode changed
what `PageDown` meant and nothing else.** The scroll area still held the whole document, so the next
page still showed below the current one and the wheel still rolled through the lot. §6c fixed the
page *counter* in that mode, which is why the mode looked half-working rather than absent.

Naming a mode after navigation and then only implementing navigation is how that happened.

### What paged mode is now

The user's own framing, adopted directly: **a kind of view in which the window shows one page.**
Everything follows from `View::sole_page`:

| | Free | Paged |
|---|---|---|
| Scrolling column | the whole document | one page |
| Pages on screen | however many fit | exactly one |
| Scroll range | `0 .. content - window` | that page's own extent |
| Which page you are on | half-of-what-it-could-show (§6c) | the page containing the top edge |
| Wheel off the end | clamps | turns the page |

The column is the load-bearing part. egui owns the live scroll offset and clamps it to the content it
was given, so handing it **one page's worth of content** is what physically stops the wheel from
rolling into the next page. There is no fight with the scroll area and no per-frame offset override;
hand-scrolling inside a page the window cannot show all of still uses egui's own inertia.

"The page containing the top edge" is the rule §6c *rejected* — and it is right here for the reason it
was wrong there: the view is confined within one page, so the top edge is inside that page by
construction and the question is no longer ambiguous.

### Two devices, two rules for the wheel

A mouse wheel sends one discrete event per notch and a person spinning it expects a page per notch. A
trackpad sends a continuous stream, and one swipe should turn one page however many events it
contains. `input::PageTurns` reads egui's own smoothing test to tell them apart rather than
inventing a second one.

Two decisions worth recording:

- **The gesture is claimed on its first frame.** A swipe that starts with page left to read spends
  itself scrolling *that* page and does not also turn one on reaching the bottom. One gesture, one
  thing.
- **Scrolling backwards lands on the previous page's *bottom***, not its top, so that going down off
  the end of a page and straight back up returns you where you were. `PreviousPage` — PageUp — is
  the command that goes to a page's top. Scrolling and navigating are different verbs.

### The bug the harness found

The wheel is the one input the control channel cannot send, so it was driven with real
`WM_MOUSEWHEEL` messages posted at the window, with free mode as the control. Free mode scrolled;
paged mode did nothing.

The gate was *"is the pointer over the pages"* — needed so that scrolling the thumbnail strip does
not turn pages. But **a wheel event routinely arrives with no pointer position at all**: winit's
mouse-leave tracking clears egui's `hover_pos` as soon as the cursor is not where it expects, and
that test then fails closed, silently.

It is now *"is the pointer over the thumbnail strip"*, negated. Same intent, opposite failure mode:
guessing wrong scrolls the wrong panel for one gesture, instead of leaving paged mode looking exactly
as broken as it did before. `input::wheel_is_for_the_pages` is a pure function so the no-pointer case
has a test rather than a comment.

Neither the mode nor the wheel would have been caught by the existing suite: every scroll-mode test
asserted the mode *field*, which was always correct. The tests now assert what the mode does to the
view.

## 6e. The page grid and the page column were fighting over textures (2026-07-30)

Reported immediately after §6d, with a screenshot: *"pages 10, 11, 12 actually flicker."* The
last three thumbnails of a twelve-page document strobed between the page and a grey box.

### It is arithmetic, not a race

Eviction lived inside the page column: keep the textures for the visible display positions
plus `RETAIN_PAGES` = 8 either side, drop everything else. The **thumbnail grid uses the
same cache** — deliberately, so there is one pipeline and one memory budget rather than two
— and the grid shows the whole of a short document.

So on any document longer than the retain window: the grid asks for a thumbnail, the render
lands, the column evicts it because that page is far from the viewport, and the grid asks
again. Every frame, forever.

Which pages flicker is fully determined. In paged mode the column shows one page, so the
window is positions 0..9, which are source pages 1..9. Pages 10, 11 and 12 are exactly the
ones the grid can see and the column cannot — the three in the screenshot.

**Pre-existing, and surfaced by §6d.** Free mode showed two pages, so the window was
positions 0..10 and only pages 11 and 12 fell outside. Paged mode narrowed the visible range
by one page and moved the boundary with it. The measurement below reproduces it on a
ten-page document too, where the single page outside the window was flickering all along
without being noticed.

### Measured before and after

Ten-page drawing set, paged mode, grid open, forty consecutive snapshots after warm-up:

| | Textures cached | Renders in flight |
|---|---|---|
| Before | oscillating 16 ↔ 17 | never reached 0 |
| After | 17, constant | 0 |

*A settled viewer does no work* turned out to be the whole test, and it is now an end-to-end
one — confirmed to fail against the old policy before the fix was kept. All three real
drawing sets settle, including the 400-page one, and closing the grid gives its thumbnails
back rather than holding them forever.

### The fix

Eviction is a whole-frame decision with two consumers, so it now runs once per frame after
both panels have drawn, over the **union** of what each is showing. The grid reports the
source pages it drew — it is virtualized, so that is the rows on screen, not the document —
and `retain::pages_to_keep` merges them with the column's window.

Two things worth keeping in mind about that module:

- Positions and pages are different after any reorder, and the cache is keyed by page. That
  was already handled and the comment explaining why moved with the code.
- Keeping a *page* keeps every *rung* of it, which is what lets one page be held at reading
  size and thumbnail size at once. `PageCache::retain_bucket` was deleted earlier for
  contradicting that; the page-window predicate had the same effect for anything outside the
  window and nobody noticed, because the trap was in the caller rather than in the cache.

Scrolling is unaffected: the 400-page set still benchmarks at p50 16.66 ms per frame with
0.14 ms of our own time.

## 6f. The grid had no retry budget (2026-07-30)

Flagged while fixing §6e and fixed next. The page column gave up on a page after three
attempts; the grid had no budget at all, so a page the renderer refuses was re-requested from
the grid on **every frame it was on screen**.

Measured: 120 frames with the grid open on a document containing one unrenderable page
produced **119 refusals** without the budget and **1** with it. A worker rasterizing a doomed
page sixty times a second, for as long as the panel stayed open.

### The fix is one copy, not two correct copies

Adding the missing check to the grid's copy would have fixed the symptom and left the shape
that produced it — the same shape that produced §6e, one panel applying a policy to a
resource two panels share. Both call sites now go through `queue::RenderQueue`, whose fields
are **private**, so `want` is the only way to reach the worker pool. The grid cannot skip the
policy because it cannot see the pool. That is the same enforcement the command model uses:
make the wrong thing unrepresentable rather than remembered.

Sharing it also puts the rasterization *scale* in one place, which matters more than it
looks. A caller derives the scale from a zoom rung and tags the job with that rung; if the two
ever disagreed the result would be cached under a size it was not drawn at, and that shows up
as a soft or oversized page rather than as an error.

### A smaller flaw found on the way

Deciding and spending were one step, so a refused submission — a full queue, a poisoned
lock — burned a retry on an attempt no worker ever looked at. The retry is now spent once a
worker has actually taken the job. Same three attempts, but all three reach a worker.

### Producing a deterministic failure

Harder than expected, and worth recording: hayro is robust enough that corrupt content
streams render blank rather than failing, and timeouts are not deterministic. What works is
**geometry**. A 100 x 4,000,000 pt page comes to 64 x 2,593,679 px even at the tiny zoom
thumbnails use, past the backend's 65,535 px per-axis limit — and a refused size is not worth
retrying, so it fails identically every time. `pdf_with_page_sizes` in the testkit exists for
that; `multi_page_pdf` now delegates to it.

That page also sits outside the column's prefetch window at the top of the document, so only
the grid ever asks for it — which isolates the path under test rather than testing both at
once.

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
3. **Corpus sourcing.** Still open, but no longer an M6 blocker — M6 shipped on a synthesized
   fixture plus mutation, which needs no corpus at all. What a corpus would buy now is breadth of
   *real* documents: pdf.js and PDFBox regression suites are the obvious starting points; check
   their licenses before vendoring.

## 8. Known unknowns

Flagged rather than glossed, because each is a real risk:

- **No independent hayro correctness comparison exists.** Its breadth is inferred from corpus size
  and Typst adoption. An earlier draft said M6 would replace that inference with data; it did not,
  because the comparison M6 planned was against PDFium and that was dropped by decision (§1). So
  the inference stands, deliberately. What would actually narrow it is spot-checking real documents
  against a reader a human trusts — not a second engine wired into CI.
- **hayro's documented gaps:** knockout groups, non-embedded CID fonts. Type 3 fonts render but
  [lack character-code access](https://github.com/LaurenzV/hayro/issues/1331) — that is a text
  extraction problem for Goal 4, not a Goal 1 one.
- **Mesh shadings (PDF types 4–7)** — could not confirm whether hayro implements them.
- **12- and 16-bit JPEG** — `zune-jpeg` hard-rejects any precision other than 8 bits, and no
  pure-Rust alternative was found. Rare in the wild, but a real hole.
- **`vello_cpu` self-describes contradictorily** — its own README says "ready for production use
  cases" while `sparse_strips/README.md` says the family is "not production-ready." We inherit
  this transitively through hayro either way.

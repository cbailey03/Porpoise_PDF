# Porpoise PDF

A PDF viewer and editor written in Rust, with no C PDF or codec library in the shipped binary.

See [GOALS.md](GOALS.md) for what we're building and
[docs/goal-1-plan.md](docs/goal-1-plan.md) for the stack decisions, project structure, and
milestone plan.

**Current state: M5 — Goal 1 is feature-complete.** Open a PDF, scroll it freely or page by page,
navigate by keyboard, and zoom by wheel, pinch, key, or fit mode. Pages rasterize on worker threads
so the UI never waits: a 400-page drawing set sits at around 16 MB of cached page textures rather
than four hundred pages' worth, and a synthetic scroll of 40 pages per second holds a 60 fps median
with our own code using under 15% of the frame budget.

Hardened against damaged input: 4,000 deterministic mutations plus every possible truncation length
of a valid PDF, all of which either open or return an error — none panic, none hang, and rejecting
damaged input averages 7 µs.

Still missing: a file dialog — a path is required.

Rendering fidelity is validated against real documents and for determinism, not against another
engine. Comparing output to PDFium is explicitly a non-goal: building this in pure Rust is the
objective, so the C++ engine we declined is not the yardstick. See `docs/goal-1-plan.md`
sections 1 and 6a.

## Keys

| | |
|---|---|
| `PageDown` / `Space` | Next page, or next screenful in free mode |
| `PageUp` / `Shift+Space` | Previous |
| `Home` / `End` | First / last page |
| `↑` / `↓` | Small scroll step |
| `Ctrl` + wheel, or pinch | Zoom |
| `Ctrl` `+` / `Ctrl` `-` | Zoom by one step |
| `Ctrl` `0` / `Ctrl` `1` / `Ctrl` `2` | Fit width / 100% / fit page |

## Crates

| Crate | Role |
|---|---|
| `porpoise-doc` | Opens a PDF; page count and per-page geometry. Knows nothing about rendering. |
| `porpoise-render` | Rasterizes pages to RGBA, behind a swappable `Renderer` trait. |
| `porpoise-view` | GUI-agnostic viewport logic: scroll layout, virtualization, cache policy. |
| `porpoise-app` | The `porpoise` binary. |
| `porpoise-testkit` | Fixtures, pixel diffing, and (from M6) the PDFium differential oracle. |

## Building

Requires Rust 1.97.1, which `rust-toolchain.toml` selects automatically. The MSRV floor is 1.92.

```bash
cargo build --workspace
```

## Using it

Open a PDF in the viewer:

```bash
cargo run -p porpoise-app -- path/to/file.pdf
```

Open it scrolled to a particular page:

```bash
cargo run -p porpoise-app -- path/to/file.pdf --start-page 200
```

Report page count, page sizes, and the scroll layout a viewer would build:

```bash
cargo run -p porpoise-app -- info path/to/file.pdf
```

Rasterize a page to a PNG:

```bash
cargo run -p porpoise-app -- render path/to/file.pdf --page 1 --dpi 150 -o page1.png
```

Page numbers start at 1. `--dpi` is a friendlier spelling of `--scale`, where `--scale 1.0` is
72 DPI; the two conflict and cannot be combined.

Two flags exist because a PDF is untrusted input, and both have sane defaults:

- `--max-pixels` refuses a render above a pixel budget, defaulting to 64 megapixels. A page can
  be within the per-axis limit on both axes and still be an absurd allocation — a 200x100 pt page
  at 5000 DPI is 2.5 *billion* pixels — so the total is capped, not just the dimensions.
- `--timeout-ms` gives up on a page after a time budget, defaulting to 10 seconds. Some malformed
  documents make the interpreter loop rather than crash, and memory safety does not help there.

## Checks

These are the same gates CI runs, in the order it runs them:

```bash
cargo fmt --all --check
```

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

```bash
cargo test --workspace --all-features
```

`cargo-deny` enforces the license allowlist and blocks the AGPL/GPL traps documented in
`docs/goal-1-plan.md` section 6, along with C codec libraries:

```bash
cargo deny check bans licenses sources advisories
```

## Conventions

- `unsafe_code` is `forbid`den workspace-wide. The security argument for this project rests on
  memory safety, so it is a machine-checked invariant rather than an intention.
- `unwrap`/`expect` warn in library code. Panicking on untrusted input is a denial-of-service
  bug in a PDF viewer, not a style question.
- Untrusted input is parsed and rasterized inside `catch_unwind`; a malformed page must degrade
  to one broken page, never take down the process.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

The dual license is the Rust ecosystem norm, and both halves earn their place here. Apache-2.0
carries an explicit patent grant, which MIT lacks — that matters more than usual for a PDF
implementation, since the format's image codecs have a long patent history. Offering MIT
alongside it keeps the code usable by GPLv2 projects, which Apache-2.0 alone is incompatible
with. It also matches `hayro`, our primary dependency.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you shall be dual licensed as above, without any additional terms or conditions.

### Third-party notices

`hayro` is Apache-2.0 and carries its own `NOTICE.md`, covering code adapted from PDFBox, pdf.js,
and the `png` crate. Apache-2.0 section 4(d) requires propagating those notices in any
distribution that includes the work, so a generated `THIRD-PARTY-NOTICES` file needs to land
before we ship binaries. `cargo about` is the usual tool. Not required while the only artifact is
source.

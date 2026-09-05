# Porpoise PDF

A PDF viewer and editor written in Rust, with no C PDF or codec library in the shipped binary.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development workflow, checks, conventions, and
driving the program from another process.

## Features

- **View**: scroll freely or page by page, navigate by keyboard, zoom by wheel, pinch, key, or fit
  mode, and pan once a page is wider than the window. Pages rasterize on worker threads so the UI
  never blocks — a 400-page drawing set holds about 16 MB of cached textures and scrolls at 40
  pages/sec with a 60 fps median.
- **Hardened against damaged input**: 4,000 deterministic mutations plus every truncation length of
  a valid PDF all open or return an error — none panic, none hang — and rejecting damaged input
  averages 7 µs.
- **Open a file** by dragging it onto the window, `Ctrl+O`, the **Open…** button, or a path on the
  command line. Launching with no path opens an empty window instead of refusing to start.
- **`Ctrl+T`** opens a grid of page thumbnails with two tabs: **Navigation** jumps the main view to
  a clicked page, **Reorganize** lets you pick pages and drag them into a new order. A search box
  above narrows the grid by page number — a single number, a range like `5-9`, a list like
  `1,4,7`, or any mix.
- **Reorganize** supports picking several pages at once — click, `Ctrl+click`, `Shift+click`, or a
  drag box over empty space — and moves or deletes the whole group as one undo step.
- Pages also reorder and delete from the toolbar or keyboard, with undo, then **Save** or **Save
  As**. A save is atomic (written beside the file, then renamed into place). A document whose page
  tree is nested is flattened on the way out, with each page's inherited size, rotation and
  resources written onto the page first, so moving a page between branches cannot change how it
  renders.
- Closing the window or opening another file with unsaved page changes **asks first** — save,
  discard, or cancel — the same whether a person or a script is driving.
- **Paged** and **Free** view modes: one page at a time, or one continuous scroll.

## Keys

| | |
|---|---|
| `Ctrl` `O` | Open a PDF (or drag one onto the window) |
| `Ctrl` `↑` / `Ctrl` `↓` | Move this page earlier / later |
| `Ctrl` `T` | Show or hide the page grid |
| `Ctrl` `Z` | Undo the last page edit |
| `Ctrl` `S` | Save over the original |
| `PageDown` / `Space` | Next page, or next screenful in free mode |
| `PageUp` / `Shift+Space` | Previous |
| `Home` / `End` | First / last page |
| `↑` / `↓` | Small scroll step, or the next page in paged mode |
| `←` / `→` | Pan sideways, once zoomed in past the window's width |
| `Ctrl` + wheel, or pinch | Zoom |
| `Ctrl` `+` / `Ctrl` `-` | Zoom by one step |
| `Ctrl` `0` / `Ctrl` `1` / `Ctrl` `2` | Fit width / 100% / fit page |

## Crates

| Crate | Role |
|---|---|
| `porpoise-doc` | Opens a PDF; page count and per-page geometry. Knows nothing about rendering. |
| `porpoise-render` | Rasterizes pages to RGBA, behind a swappable `Renderer` trait. |
| `porpoise-view` | GUI-agnostic viewport logic: scroll layout, virtualization, cache policy, and the command model. |
| `porpoise-app` | The `porpoise` binary. |
| `porpoise-testkit` | Fixtures, pixel diffing, and the malformed-input mutation harness. |

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

`--dpi` is a friendlier spelling of `--scale` (`--scale 1.0` is 72 DPI); the two conflict and
cannot be combined.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

The dual license is the Rust ecosystem norm. Apache-2.0 adds an explicit patent grant, which
matters more than usual here given PDF image codecs' long patent history; MIT keeps the code
usable by GPLv2 projects that Apache-2.0 alone can't satisfy. It also matches `hayro`, the primary
dependency.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you shall be dual licensed as above, without any additional terms or conditions.

### Third-party notices

`hayro` is Apache-2.0 and carries its own `NOTICE.md`, covering code adapted from PDFBox, pdf.js,
and the `png` crate. Apache-2.0 section 4(d) requires propagating those notices in any
distribution that includes the work, so a generated `THIRD-PARTY-NOTICES` file needs to land
before we ship binaries. `cargo about` is the usual tool. Not required while the only artifact is
source.

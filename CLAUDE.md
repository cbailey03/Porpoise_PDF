# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

There is no CI: none of this runs automatically. Run what's relevant before a commit that matters.

```bash
./scripts/check.sh                                                         # every check below, in one pass

cargo build --workspace                                                    # build
cargo run -p porpoise-app -- path/to/file.pdf                              # open the viewer
cargo run -p porpoise-app -- info path/to/file.pdf                         # page count / geometry
cargo run -p porpoise-app -- render path/to/file.pdf --page 1 -o out.png   # rasterize one page
cargo run -p porpoise-app -- serve path/to/file.pdf                        # stdio control protocol

cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features                                      # unit + most integration tests
cargo test -p <crate> <test_name>                                          # a single test, e.g. -p porpoise-app page_one_is_index_zero

PORPOISE_E2E=1 cargo test --workspace --all-features                       # also runs the windowed e2e tests (skipped otherwise)
cargo +1.92 check --workspace --all-features --all-targets                 # the MSRV floor still has to build
cargo deny check bans licenses sources advisories                          # license allowlist
cargo tree --package porpoise-app --edges normal | grep -iE "pdfium|mupdf|openjpeg|jpeg2k|jbig2dec|testkit"  # no output = pass
```

The e2e suite (`crates/porpoise-app/tests/control.rs`) launches the real binary and opens a real
window, so it needs a display and is skipped unless `PORPOISE_E2E=1` is set. A plain `cargo test`
reports it as passing without running it. Those tests serialize themselves on an internal mutex
(one window at a time), so no `--test-threads=1` is needed even though they're windowed.

## Architecture

Five crates, dependencies flowing one way: `porpoise-doc` and `porpoise-render` sit at the bottom,
`porpoise-view` builds on `porpoise-doc`, and `porpoise-app` (the `porpoise` binary) wraps all
three. `porpoise-testkit` is dev-only and reachable from nothing that ships.

- **`porpoise-doc`** opens a PDF and describes page geometry, using `hayro`. It's the only crate
  that also touches `lopdf`, used solely to write a reordered document back out (`save.rs`).
  `PageOrder` holds an edit as a permutation of *source* page indices. The file on disk is never
  touched until save, so undo is a cheap snapshot of that permutation rather than an inverse
  operation.
- **`porpoise-render`** rasterizes pages behind a `Renderer` trait (`HayroRenderer` is the only
  impl). `RenderPool` runs workers off the UI thread; `render_with_timeout` bounds a hung page by
  abandoning its worker thread rather than blocking the pool (Rust can't cancel a running thread,
  so a hang leaks one thread rather than corrupting state).
- **`porpoise-view`** is GUI-agnostic viewport logic with no window or renderer dependency: scroll
  layout, virtualization, cache policy, and zoom, organized one module per question (`layout`,
  `fit`, `zoom`, `cache`, `request`, `page`, `viewport`). `ViewCommand` plus `apply()` is the single
  path that changes `ViewState`; nothing else is allowed to.
- **`porpoise-app`** is the eframe/egui shell. It defines its own `Command` enum that wraps
  `ViewCommand` and adds effects that need I/O (`Open`, `Close`, `Capture`, `SaveAs`, ...).
  `Viewer::dispatch` in `viewer.rs` is the *only* thing that carries a `Command` out. Keyboard
  input (`input.rs`), toolbar/page-grid clicks, and the control-channel protocol
  (`protocol.rs`/`control.rs`) are all just producers of the same enum. That's what makes "every
  feature is programmatically controllable" a structural property instead of a maintained one.
  `viewer.rs` and `chrome.rs` hold the stateful, untested-by-necessity parts (they need a live
  `egui::Context`); most other modules in the crate (`edits`, `confirm`, `failure`, `label`,
  `thumbnails`, `saver`, `picker`) are pulled out specifically because they're pure enough to unit
  test.

**Two numbering conventions, each with exactly one legal crossing point.** This codebase has been
bitten by conflating them more than once, so the conventions are load-bearing:

- `PageNumber` (1-based; anything a person or agent can see) vs. a bare `usize` index (0-based;
  anything indexing a collection). `PageNumber::index` / `PageNumber::from_index` are the only
  conversions.
- Display *position* vs. *source* page index, which differ once pages have been reordered.
  `PageOrder::source_of` is the only crossing; variables are named `position` or `source`, never
  `page`.
- Screen pixels vs. PDF points, documented and converted in `porpoise-view::viewport`.

**Commands are matched exhaustively three separate times**: naming a command, advertising it, and
deciding whether it discards unsaved work. That's deliberately not deduplicated, so adding a
`Command` variant fails to compile until all three are updated.

**The unsaved-changes guard sits in front of `dispatch`, not on individual gestures.** The X
button, `Ctrl+O`, a file drop, and an agent's `open`/`close`/`quit` all funnel through the same
check, so a person and a script get identical protection from one code path (see
`crate::confirm`).

**The control protocol** (`porpoise serve`, newline-delimited JSON on stdio, opt-in and never a
network port) is hand-decoded rather than derived with `serde`, so a malformed command can name
what's wrong rather than failing as "no variant matched." `idle` is emitted on the falling edge,
not as a level. An edit that needs no new rasterization never leaves the settled state, so there
is no `idle` event to wait for; wait on the specific completion event instead (e.g.
`pages_reordered`).

**Untrusted input**: rendering runs inside `catch_unwind`, bounded by `--max-pixels` and
`--timeout-ms`. `unsafe_code` is `forbid`den workspace-wide, and `unwrap`/`expect` warn outside
tests (`clippy.toml` allows them in tests, since a panic is the clearest way to write one).

## Code Preference
- Prefer simple, long-term stable solutions.
- Document complex decisions.
- **Plain ASCII only:** every text file in the repo (code, comments, docs, config) uses plain ASCII characters. No em or en dashes, curly quotes, degree signs, superscripts, or emoji. Write `-`, `"`, `degrees`, `m2` instead.

### Design principles

- **Composition over inheritance:** Build features by combining small objects ("has-a") rather than making rigid parent-child trees ("is-a").
- **Interfaces at real seams only:** Define a trait where behavior genuinely varies or code touches the outside world (rendering, disk, the control channel). Everywhere else, keep code concrete until a second implementation actually exists.
- **Encapsulate what varies:** Isolate volatile logic into its own modules to protect stable code from changes.
- **Single Responsibility (SRP):** Give every class, module, or function exactly one job and one reason to change.
- **Open/Closed (OCP):** Write code that lets you add new behavior without rewriting or breaking existing, tested code. The exception is where the Architecture section deliberately requires edits, such as the three exhaustive matches a new `Command` variant must update.
- **Liskov Substitution (LSP):** Any implementation of a trait must honor its full contract, so it can replace another without breaking callers.
- **Interface Segregation (ISP):** Build small, specific interfaces instead of bulky ones so code only depends on what it actually uses.
- **Dependency Inversion (DIP):** Make high-level logic depend on abstract interfaces, not low-level technical details.
- **Law of Demeter:** Do not chain deep object calls like `a.get_b().get_c().do_it()`. Talk only to immediate neighbors. Builder and iterator chains (`ui.button(..).on_hover_text(..)`, `iter().map(..)`) don't count; they act on one value, not a walk through other objects.

## Git
- **Every PR targets `main` directly**, and must leave the application in a
  working state when merged. No exceptions by issue type.
- Every branch is named `issue-<number>-<github-username>`, using the GitHub issue
  number with no zero padding (e.g. `issue-15-cbailey03`, `issue-2416-cbailey03`).
  There are no exceptions; releases are tags, not branches.
- **Local work:** commit as often as convenient. Nothing here restricts local
  commit frequency. Messy WIP commits are fine, they never reach permanent
  history.

**Before opening a PR:**
1. Squash all local commits into one.
2. Rebase that commit onto the latest main:
   ```bash
   git fetch origin
   git rebase origin/main
   ```
   Do not `git pull` main into the issue branch. That creates a merge commit
   and breaks the single-commit rule.
3. Write that single commit's message following Django's rules:
   - Subject line in past tense, ending with a period.
     - Right: `Fixed the customer name uniqueness check.`
     - Wrong: `Fix the check.` / `Fixing the check.` / `Fixed the check` (no period)
   - Blank line, then a body wrapped at ~72 chars per line (soft limit).
   - The body explains **why**, not what or how. The diff already shows
     that.
   - The body is 5 sentences or fewer, since it doubles as the PR Summary.
4. Open the PR using `.github/pull_request_template.md` and fill in every
   section. `gh pr create --body` does not apply the template, so build the
   body from its sections.
   - **Summary** is that same commit body, verbatim. Write it once, not
     twice.
   - **Related Issue** is `Closes #<number>`, matching the branch's issue
     number, so the issue closes on merge.

# Porpoise PDF

A PDF viewer and editor written in Rust, with no C PDF or codec library in the shipped binary.

See [GOALS.md](GOALS.md) for what we're building and
[docs/goal-1-plan.md](docs/goal-1-plan.md) for the stack decisions, project structure, and
milestone plan.

**Current state: M0.** The workspace, CI, and license gating are in place. Page geometry and
scroll layout work; there is no window yet — that arrives at M2.

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

Report what the viewer would lay out for a document:

```bash
cargo run -p porpoise-app -- path/to/file.pdf
```

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

`Cargo.toml` declares `MIT OR Apache-2.0`, but the `LICENSE-MIT` and `LICENSE-APACHE` files are
not written yet — that needs a decision on the copyright holder. See
`docs/goal-1-plan.md` section 7.

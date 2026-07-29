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

# Contributing

See [README.md](README.md) for what Porpoise PDF is and how to build and run it.

## Checks

**There is no CI.** These run locally, and nothing runs them for you — so a commit is only as
checked as whoever made it. Removed deliberately while the commit rate is high; the workflow is in
git history if it earns its place back.

Run all of them before a commit that matters. One command does that:

```bash
./scripts/check.sh
```

It keeps going after a failure and summarizes at the end, so one run tells you everything that is
wrong instead of only the first thing. A check whose tool is not installed is reported as a failure
with the command that installs it, never skipped quietly. The individual checks follow, in the order
the script runs them.

```bash
cargo fmt --all --check
```

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

```bash
cargo test --workspace --all-features
```

`cargo-deny` enforces the license allowlist and blocks the AGPL/GPL traps, along with C codec
libraries:

```bash
cargo deny check bans licenses sources advisories
```

Three more that used to run only in CI, and are easy to forget for that reason.

The end-to-end tests open a real window and are skipped unless asked for, so a plain `cargo test`
reports them as passing without running them:

```bash
PORPOISE_E2E=1 cargo test --workspace --all-features
```

The oldest floor still has to build:

```bash
cargo +1.92 check --workspace --all-features --all-targets
```

And the one that guards this project's central claim — no C PDF or codec library in the shipped
binary. `deny.toml` bans the known names, but this catches anything arriving by a new path, including
a test-only dependency leaking out of `porpoise-testkit`:

```bash
cargo tree --package porpoise-app --edges normal | grep -iE "pdfium|mupdf|openjpeg|jpeg2k|jbig2dec|testkit"
```

No output is a pass.

## Commit messages

Follows Django's commit message conventions:

- Written in the past tense, and ends with a period.
- Each line wrapped at 70 characters.
- Explains *why* the change was made, not what changed. The diff already shows what.

## Conventions

- `unsafe_code` is `forbid`den workspace-wide. The security argument for this project rests on
  memory safety, so it is a machine-checked invariant rather than an intention.
- `unwrap`/`expect` warn in library code. Panicking on untrusted input is a denial-of-service
  bug in a PDF viewer, not a style question.
- Untrusted input is parsed and rasterized inside `catch_unwind`; a malformed page must degrade
  to one broken page, never take down the process.
- A page that times out is retried a bounded number of times, because a timeout usually means the
  machine was busy. A page that panics or is refused for its size is not retried — that failure is
  deterministic, so a retry only spends a worker to reach the same answer.

## Diagnostics

Warnings go to stderr, so they never mix with `info` and `render` output. Set `RUST_LOG` to a level
to see more:

```bash
RUST_LOG=debug porpoise file.pdf
```

`trace`, `debug`, `info`, `warn` (the default), `error`, and `off` are understood. A per-target
directive like `RUST_LOG=porpoise_render=debug` is *not* parsed — it falls back to the default rather
than going silent — because supporting it means pulling regex machinery in to parse a filter string.

## Untrusted input

Two flags exist because a PDF is untrusted input, and both have sane defaults:

- `--max-pixels` refuses a render above a pixel budget, defaulting to 64 megapixels. A page can
  be within the per-axis limit on both axes and still be an absurd allocation — a 200x100 pt page
  at 5000 DPI is 2.5 *billion* pixels — so the total is capped, not just the dimensions.
- `--timeout-ms` gives up on a page after a time budget, defaulting to 10 seconds. Some malformed
  documents make the interpreter loop rather than crash, and memory safety does not help there.

## Page numbers

Page numbers start at 1 everywhere they are visible: the CLI, the status bar, the control protocol,
and every event. There is no zero-based page number anywhere a person or an agent can see one, and
`{"page":0}` is refused rather than quietly meaning page 1.

Internally, page *indices* start at 0, because they index arrays. The two are separate types —
`PageNumber` and `usize` — so converting between them has to be written down. That is not
pedantry: the protocol shipped with `go_to_page` counting from 0 while `--start-page` counted from
1, in the same program.

## Driving it from another program

Every effect in the viewer is reachable by a named command, so a script or an AI
agent can operate it. `porpoise serve` opens a window and reads newline-delimited
JSON on stdin, replying and reporting events on stdout:

```bash
porpoise serve document.pdf
```

```text
in   {"id":1,"command":"go_to_page","page":4}
out  {"id":1,"ok":true,"outcome":"changed"}
out  {"event":"page_rendered","page":4}
out  {"event":"idle"}
in   {"id":2,"command":"capture","path":"page5.png"}
out  {"id":2,"ok":true,"outcome":"capturing"}
out  {"event":"captured","path":"page5.png"}
```

Send `{"command":"commands"}` for the full list and `{"command":"snapshot"}` for the
current state. The file argument is optional — send `{"command":"open","path":"…"}`
instead.

Four things worth knowing:

- **Wait for `idle` before capturing or asserting.** It means nothing is queued and
  everything visible is drawn. Acting before it gets you placeholder tiles.
- **Page numbers start at 1**, here and everywhere else. `{"page":0}` is refused.
- **`quit`, `close` and `open` can come back `needs_answer`** when pages have been
  reordered and not saved. Nothing has happened yet; read `awaiting_answer` in the
  snapshot to see what is being asked, then reply with
  `{"command":"answer","choice":"save"|"discard"|"cancel"}`. You get the same
  protection a person does, for the same reason.
- **Closing stdin exits the program**, the way every other stdio protocol behaves —
  including with unsaved changes, because by then there is nobody left to ask.

This is off unless you ask for it, and it is stdio only — no port is opened. Be
clear about what you are granting: the controlling process can open any file you can
read, see it rendered, and write a PNG anywhere you can write. It runs as you
already, so this is not an escalation, but it is more than "a viewer".

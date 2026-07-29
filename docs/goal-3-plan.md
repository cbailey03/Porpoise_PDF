# Goal 3 — Open a file from inside the program

Status: **complete**. Milestones M11–M13 below.

Today `porpoise` refuses to launch without a path:

```
no file given — try `porpoise <file.pdf>`, or `porpoise --help`
```

That is the last thing in the viewer that a person cannot do from inside the program, and it is the
first thing anyone tries. This goal closes it.

## 1. What this is, and what it is not

It is three things:

1. A dialog for choosing a file, reachable from a toolbar button and `Ctrl+O`.
2. A window that starts empty and waits, instead of exiting with an error.
3. A visible failure message, because a picker makes "that file would not open" a normal user-facing
   outcome for the first time.

The third is not padding. Until now a bad path was a startup error printed to a terminal. Once
someone can choose a file at runtime, `Document::open` failing has to land somewhere they can see —
and right now it goes to `tracing::warn!` on stderr, which for a windowed app means nowhere.

### The dialog is deliberately *not* a command

Goal 2's rule is *effects, not gestures* (`goal-2-plan.md` §1). A dialog is not an effect on the
document; it is a way of authoring the argument to one. So the picker joins the keyboard and the
toolbar as a **producer** of `Command::Open { path }`, and no `pick_file` command is added to the
control protocol.

This is worth stating because it looks like an exception to "every feature is programmatically
controllable" and is not:

- An agent already has `open` with a path, which is strictly *more* capable than a dialog — it can
  open a file without a human present.
- Exposing `pick_file` would let an agent enter a state it cannot leave. A native modal is dismissed
  by a person; an agent that opened one and then found it had no `cancel_pick` would have hung the UI
  it was supposed to be driving. Adding a capability whose only exit is human intervention would
  weaken the invariant it appears to serve.

If we ever do expose it, it has to arrive with programmatic cancellation in the same change.

### Explicitly not in Goal 3

- **Drag and drop.** A natural second producer of `open` and genuinely cheap with egui's
  `dropped_files`, but it is a separate input path with its own edge cases (directories, multiple
  files, non-PDFs). Not folded in silently.
- **Recent files.** Wants persistence, which the program has none of yet.
- **A save dialog.** Nothing to save until editing.
- **Multiple documents open at once.** `OpenDocument` is deliberately a single `Option`.
- **An in-window file browser.** See the decision below.

## 2. Native dialog, and the dependency question

The obvious concern for this project is dependencies: the whole argument rests on a small, auditable,
pure-Rust graph. A file dialog is exactly where that can go wrong, because the usual crate reaches for
GTK on Linux.

`rfd` 0.17's default features are `xdg-portal` + `wayland` + `pollster`. `gtk3`, `gtk-sys`,
`glib-sys` and `gobject-sys` are all **off by default**, so the C toolkit is opt-in and we do not opt
in. On Windows it is COM through `windows-sys`, which `eframe` already pulls in via `arboard`. So on
the platform this is being developed on, the dialog adds no new native surface at all.

The alternative considered was an in-window file browser drawn in egui: no new dependency, identical
on every platform, and trivially testable. Rejected because a file dialog is one of the few places a
person's muscle memory is entirely about the *system* dialog — typeahead, sidebar places, network
locations, their own bookmarks. Reimplementing a worse one to save a dependency that turns out to be
pure Rust anyway is the wrong trade.

### It must not block the frame loop

`rfd::FileDialog::pick_file()` blocks until the person chooses. Calling that from `App::ui` freezes
rendering, which breaks the one property this viewer has been careful about throughout — the UI never
waits.

`rfd` also offers `AsyncFileDialog`, but that wants an executor and this program has no async runtime.
Rather than adopt one for a dialog, the picker runs the blocking call on a `std::thread` and sends the
result back over a channel, polled once per frame. That is the same shape as the render pool, so it
introduces no new concurrency concept.

**Known risk:** on macOS, `NSOpenPanel` must run on the main thread, so the thread-plus-channel
pattern is wrong there. We do not build or test macOS today. Recorded rather than papered over; the
fix when it matters is `AsyncFileDialog` plus an executor, or a main-thread pump.

## 3. Where the failure message goes

`DispatchResult::Failed(String)` already exists and already reaches the control channel as a reply.
It has no user-visible path. Adding one:

- `Viewer` holds `last_error: Option<String>`, set by any failed dispatch and cleared by the next
  successful open or close.
- The status bar renders it.
- `Snapshot` carries it, so **anything a person can see, an agent can read** — the same rule §3 of the
  Goal 2 plan set for the rest of the view state. Without this the two paths would report failure
  differently, which is exactly the drift the command model exists to prevent.

Not a modal. A modal would need its own dismissal command to stay programmatically controllable, and
the same reasoning that keeps `pick_file` out of the protocol applies.

## 4. Milestones

| | | |
|---|---|---|
| **M11** | Empty window and a visible error surface | ✅ |
| **M12** | The picker itself, on a worker thread | ✅ |
| **M13** | Tests, and honesty about what cannot be tested | ✅ |

**M11.** `run_viewer` stops erroring on a missing path and opens an empty window — the `serve`
subcommand already did this, so the two paths converge. `last_error` added to `Viewer` and to
`Snapshot`, rendered in the status bar.

**M12.** `FilePicker` owns the pending pick: a channel, a "already asking" flag so a second `Ctrl+O`
cannot stack dialogs, and a `poll` that returns `Option<PathBuf>`. Toolbar **Open…** button and
`Ctrl+O` both produce the same request. A chosen path becomes `Command::Open { path }` through the
normal dispatch, so it emits `DocumentOpened` and reaches the control channel like any other open.

**M13.** The pure parts are unit-tested: the picker's state machine with a stubbed channel, the error
surface being set and cleared, and `Ctrl+O` mapping. An end-to-end test covers launching with no
document and opening by command.

**What cannot be tested, stated plainly:** a native modal cannot be driven headlessly, so "the dialog
appears and returns a path" has no automated test. The seam is placed so that everything *except* the
`rfd` call is covered — `poll` is tested against a channel we fill ourselves. The `rfd` call itself is
verified by hand, and that limitation is real rather than hidden behind a test that asserts something
weaker than it appears to.

## 4a. What building it changed

**An empty window could not be captured.** `drive_screenshot` gated on
`open.settled() && !cache.is_empty()` — with no document that is false forever, so the request was
never sent and the attempt burned its entire 240-frame budget before reporting "no screenshot
arrived". Harmless while a path was mandatory; since M11 an empty window is *how the program starts*,
which makes it the first thing anyone would try to capture. `is_some_and` became `is_none_or`: with no
document there is no pipeline to wait for.

Found by asking how to verify M11 rather than by a test — the capture was going to be the evidence,
and it did not work. Third time in this project that a state which had never been reachable before
turned out to be mishandled the moment it became reachable.

**The empty-window text was stale advice.** It read *"Pass a path, or send an `open` command"* — both
true, neither what a person in front of the window should do. Now *"Press Ctrl+O, or pass a path on
the command line."* Small, but the kind of drift the Goal 1 audit was about: the text was accurate
when written and became wrong without changing.

**A failed open must not close the working document.** Not something the plan said, and easy to get
wrong given `Command::Open` replaces `self.open` wholesale. It happens to be right — the failure
returns before the assignment — so the end-to-end test now pins it rather than leaving it to luck.

## 5. Known unknowns

- Whether a background-thread dialog on Windows behaves as a properly owned modal, or can end up
  behind the main window. To be checked by hand.
- Whether cancelling the dialog is distinguishable from an error. It should be a plain no-op with no
  message; `pick_file` returning `None` covers both cancel and failure-to-show, and we treat both as
  "nothing happened".

# Goal 2 — Complete programmatic control

**Status: complete.** M7–M10 all shipped 2026-07-29. Written immediately after the Goal 1 audit
(`goal-1-plan.md` §6b) and before any code moved; §10 records what building it actually taught us,
including two design errors this document got wrong.

Goal 1 built a viewer a person can use. Goal 2 makes every part of it reachable from code, so an AI
agent can operate the program on a user's behalf. This document argues the design before the
refactor, because the refactor moves state that currently lives in five places into one.

---

## 1. What "complete control" means

The literal reading — *every* interaction is a command — is unachievable and not what we want. A
trackpad pinch is not a command; it is a device producing a continuous stream. Demanding a
`PinchZoom` command would mean inventing commands for gestures, and then quietly making exceptions.

The rule we hold instead:

> **Every observable effect is reachable by a named command. Input devices stay input devices.**

A pinch *is* `SetZoom(Fixed(1.4))`. A wheel scroll *is* `ScrollBy { points: 120.0 }`. A click on
"Fit Width" *is* `SetZoom(FitWidth)`. The device translates to a command and the command is the only
thing that changes state. So there is nothing an agent cannot do that a person can, while we never
have to model fingers.

The corollary is the part that shapes future work: **every effect must be addressable without
pixels.** Search results become "result 3 of 12", not "the highlight at (420, 880)". Text selection
becomes a character range. Annotations get identities. Each of those is a decision that would
otherwise default to the pixel form, because pixels are sufficient when a human with a mouse is the
only user you imagine. This constraint is also why the same work serves the test-coverage goal:
something addressable by name is something assertable by name.

### Explicitly not in Goal 2

- **Editing.** That becomes Goal 4, after the file picker in Goal 3. Note the ordering benefit: every
  edit operation will have to be a command anyway, so doing the command model first means editing is
  built on it rather than retrofitted into it.
- **A network-reachable API.** See §5.
- **An embedded scripting language.** A command model plus a transport covers the need; a scripting
  runtime is a much larger dependency and answers no question we have.
- **Simulated input.** We do not want an agent moving a virtual mouse to a button's coordinates.
  That approach is the industry norm and it is brittle for exactly the reason we are avoiding it: it
  breaks when a button moves five pixels.

---

## 2. The command model

### The structural move

Today a key press calls `Viewer::go_to_page` directly, and so does the toolbar. Adding an agent
channel as a third caller would give three paths into the same state, and nothing would prevent a
fourth feature from being reachable only by clicking.

Inverting that is the whole of phase 1:

```rust
// porpoise-view — pure, no window, no document, no renderer.
pub enum ViewCommand {
    GoToPage(usize),
    NextPage,
    PreviousPage,
    FirstPage,
    LastPage,
    ScrollTo { points: f64 },
    ScrollBy { points: f64 },
    ScrollByViewports { fraction: f64 },
    SetZoom(ZoomTarget),          // FitWidth | FitPage | Fixed(f32)
    StepZoom { rungs: i16 },
    SetScrollMode(ScrollMode),
}

pub fn apply(
    state: &mut ViewState,
    layout: &ScrollLayout,
    viewport: Viewport,
    command: ViewCommand,
) -> Outcome
```

Keyboard, toolbar, and agent all become *producers* of `ViewCommand`. Once that holds, "every
feature is programmatically controllable" is structurally guaranteed rather than maintained by
discipline — you cannot write a click-only feature, because clicks produce commands and commands
are the surface.

### Two layers, because only one of them can be pure

`ViewCommand` covers what changes the view. It needs no document and no window, which is what keeps
it testable the way the rest of `porpoise-view` is. Shell-level actions cannot be pure, so they sit
in a wrapper owned by `porpoise-app`:

```rust
// porpoise-app
pub enum Command {
    View(ViewCommand),
    Open { path: PathBuf },
    Close,
    Capture { path: PathBuf },   // the current --screenshot, promoted to a real command
    Quit,
}
```

This split is deliberate. Putting `Open` into `porpoise-view` would drag document loading into the
one crate whose value comes from having no I/O.

### Separating state from environment from derived values

The refactor's real payoff is a distinction the current `Viewer` does not draw. Its 23 fields mix
three different kinds of thing, and untangling them is most of the work:

| Kind | Fields | Who owns it |
|---|---|---|
| **Authoritative state** | scroll position, zoom mode, scroll mode | `ViewState` |
| **Environment** | viewport width and height | measured from the window each frame |
| **Derived** | zoom factor, zoom rung, current page, visible range | computed, never stored |

Today `zoom`, `bucket`, and `current_page` are all stored fields recomputed every frame, which means
they can disagree with their inputs for a frame at a time. Making them derived accessors over
`(ViewState, ScrollLayout, Viewport)` removes a class of staleness bug we have not hit yet but would.

### The one real risk: who owns the scroll position

**This is the part most likely to go wrong, so it is worth deciding up front.**

egui's `ScrollArea` currently owns the live scroll offset. We read it back each frame and only write
to it on frames where navigation asked for a jump. That arrangement is why hand-scrolling feels
native — egui's own inertia and smoothing are doing the work, and that responsiveness was
specifically approved after testing on a 400-page document.

Making `ViewState` the sole owner and driving egui's offset every frame would be cleaner in theory
and would risk fighting egui's smoothing in practice. **We are not doing that.** Instead:

- `ViewState` holds a *requested* scroll position, set by commands.
- The frame loop applies any request to egui, then reads the actual offset back as the current one.
- Reconciliation happens in exactly one place, named as such.

So `apply` stays pure and testable — `GoToPage(5)` on a given layout *requests* 4020 pt, which is
assertable with no window at all — while egui keeps doing the thing it is good at. The compromise is
that an agent's scroll takes effect on the next frame rather than instantly, which is why §3's
`Idle` event exists.

---

## 3. Introspection: the half that gets skipped

An agent that can act but not observe is driving blind, and will fall back to guessing and retrying.
Two mechanisms, both needed.

### A readable snapshot

```rust
pub struct ViewSnapshot {
    pub page_count: usize,
    pub current_page: usize,
    pub scroll_top_pt: f64,
    pub content_height_pt: f64,
    pub zoom: f32,
    pub zoom_mode: ZoomMode,
    pub scroll_mode: ScrollMode,
    pub pages_visible: Range<usize>,
    pub pages_cached: usize,
    pub renders_in_flight: usize,
    pub failed_pages: Vec<usize>,
}
```

Most of this is already computed for the status bar. The status bar becomes a renderer of the
snapshot rather than a separate set of reads — which also means anything a person can see about the
program's state, an agent can read.

### An event stream, and why `Idle` matters most

Polling a snapshot in a loop works but wastes both sides' time. Events to emit:

- `DocumentOpened { path, page_count }`
- `ViewChanged { snapshot }` — coalesced to at most one per frame
- `PageRendered { page }`
- `PageFailed { page, reason, will_retry }`
- **`Idle`** — nothing queued, nothing in flight, everything visible is drawn

`Idle` is the single most valuable thing we can give an agent. An agent that issues `GoToPage(300)`
and then immediately captures the window gets placeholder tiles. Without an idle signal its only
option is to sleep and hope, which is the root of most flaky agent automation.

We already compute exactly this condition — `!pool.is_busy() && in_flight.is_empty()` — for the
`--screenshot` path, where it is the difference between capturing pages and capturing grey
rectangles. Promoting it from a private detail to a published event costs almost nothing and removes
the need for every client to reinvent it badly.

---

## 4. Transport

**Newline-delimited JSON over stdio**, behind `porpoise serve`. One JSON object per line, both
directions, with an `id` on requests so responses can be correlated and events can arrive
unsolicited.

Why this and not the alternatives:

- **stdio, not a socket.** See §5 — this is a security decision, not a convenience one.
- **NDJSON, not JSON-RPC 2.0.** JSON-RPC gives request/response correlation and notifications, which
  is what we need, wrapped in ceremony we do not. NDJSON with an explicit `id` is the same shape
  minus the envelope, and remains a compatible upgrade path if we ever want LSP-style tooling to
  attach.
- **JSON, not a binary format.** The client is likely to be a script or an agent runtime in another
  language. Legibility beats a few microseconds on a channel that carries tens of messages a second.

### New dependencies

`serde` and `serde_json`. Both are effectively universal, both MIT OR Apache-2.0, both already
transitively present via the eframe stack — so this adds no new *distinct* code to the tree, only a
direct edge.

`porpoise-view` takes serde as an **optional feature**, off by default:

```rust
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ViewCommand { .. }
```

The wire format is `porpoise-app`'s concern; the core stays dependency-light for any consumer that
does not need it. `porpoise-view` currently depends on exactly one crate and that is worth
protecting.

---

## 5. Security model

Goal 2 deliberately builds a capability to hand complete control of the program to another process.
That deserves stating plainly rather than discovering later.

**What a controlling process can do:** open any file the user can read and have it parsed and
rendered; read everything on screen; write a PNG anywhere the user can write. That is, in effect, a
**file-read capability plus a screen-read capability**. It is not a sandbox escape and it is not
privilege escalation — the controller already runs as the user — but it is meaningfully more than
"a viewer."

Therefore:

1. **Opt-in, never default.** Control exists only when `porpoise serve` is invoked. A viewer that
   always listens would let any local process drive it.
2. **stdio only. No TCP, no Unix socket, no named pipe.** A bound endpoint is reachable by every
   process on the machine and needs an authentication story we have no reason to write. stdio is a
   pipe handed to us by a parent that already holds our privileges — a different risk class
   entirely. Revisit only with a concrete need and an auth design.
3. **No command may extend the capability.** No "run this", no "load this plugin", no arbitrary
   write beyond the explicit path in `Capture`. The command set is a closed list, and §6 makes that
   list mechanically enumerable.
4. **The channel is an untrusted-input surface and gets treated like one.** This project's hardening
   argument (`goal-1-plan.md` §2, §6a) is that every parser fed outside data is fuzzed. A command
   decoder is a parser. The mutation harness grows an arm that feeds it malformed and hostile JSON,
   and the requirement is the same as for PDFs: reject, never panic, never hang.

Point 4 is easy to skip and would quietly undermine the whole security premise. A viewer that
survives 4,000 malformed PDFs and then panics on a malformed command line has moved the hole, not
closed it.

---

## 6. Enforcing completeness mechanically

"Every feature is controllable" and "we maintain complete test coverage" are both claims that rot
into intentions unless something checks them. Line-coverage percentages are a weak proxy. There is a
much better one available here.

A single exhaustive list of commands, used three ways:

```rust
impl ViewCommand {
    /// Every variant, with a representative value.
    pub const ALL: &[Self] = &[ .. ];
}
```

1. **Discovery.** The control channel publishes it, so an agent can ask what the program can do
   rather than being told out of band.
2. **Documentation.** It is the command reference, generated from the thing it documents, so it
   cannot drift — which is precisely the failure mode §6b of the Goal 1 plan just cleaned up.
3. **Coverage enforcement.** A test matches exhaustively over `ViewCommand`. Adding a variant
   **fails to compile** until it is handled, and the handler is an assertion that a behaviour test
   exists for it.

That last one is the same discipline as `unsafe_code = "forbid"`: a machine-checked invariant rather
than a remembered one. It is the strongest available answer to "is every feature really
controllable" — the build breaks if it is not.

---

## 7. Milestones

| | Deliverable | Proves |
|---|---|---|
| **M7** ✅ | `ViewCommand`, `ViewState`, `apply` in `porpoise-view`, pure and fully tested. `Viewer` rewired so keyboard and toolbar emit commands; direct method calls deleted. `Command` wrapper in `porpoise-app`. **No behaviour change** — verified by re-running the screenshot, benchmark and time-to-first-page checks and comparing. | One path into every state change. The foundation exists. |
| **M8** ✅ | `ViewSnapshot`, `Snapshot`, the event stream, and `Idle`. Status bar reads the snapshot. Still no transport. | The program can describe itself. |
| **M9** ✅ | `porpoise serve`: NDJSON over stdio, `serde` as an optional feature of `porpoise-view`, hand-written decoding with per-command errors, a line-length cap, and resynchronization after a bad line. | An outside process can drive it. |
| **M10** ✅ | Five end-to-end tests that launch the real binary and talk to it over a real pipe: navigate + capture + verify the PNG, open a document not on the command line, survive malformed input, refuse a bad page without moving, and exit when stdin closes. | **Goal 2 is real**, not asserted. |

**M10 is not optional.** Without it, "an AI agent can drive the whole program" is exactly the kind of
untested claim this project has refused everywhere else. It is also the milestone that will find the
awkward parts of the protocol, because it is the first thing to actually use it.

M7 is the one worth doing carefully and the one where nothing visible improves. Its value is entirely
that M8–M10 and every future feature become cheap. It is also the cheapest it will ever be: the
program currently has about eleven commands' worth of behaviour. After editing, annotation, search,
and forms land, the same conversion is a rewrite nobody schedules.

---

## 7a. What building it changed

Three things this document got wrong or left out, recorded because the reasoning is more useful than
the conclusion.

**`current_page` did not round-trip with `GoToPage`, and M10 caught it in its first run.** It was
defined as the page under the viewport's *centre*, which reads well in a status bar. But a viewport
taller than a page — a small page, or any page at fit-width in a wide window — has its centre inside
the *next* page. So `go_to_page(3)` scrolled correctly to 936 pt and then reported page 4. Three of
the five end-to-end tests failed on this one cause.

The fix was to anchor to the topmost visible page. Worth noting that the obvious alternative, "the
page occupying most of the view", has the same flaw: after navigating to page N with three pages on
screen, page N+1 can occupy more of the viewport than N does. Anchoring to the top is the only
definition under which navigating somewhere and asking where you are agree — which is the first
property an agent relies on, and was not a property this plan thought to state.

**A zoom change has to force a scroll request even when the position is unchanged.** Scroll position
is in points, which are zoom-independent; the shell's offset is in pixels, which are not. So after
changing zoom, our position is the same by our own measure while egui's offset now points somewhere
else in the document. Suppressing the "redundant" request left the view wherever the stale pixel
offset landed. §2's ownership split is right, but it has this consequence and the plan did not say so.

**Mode-dependence belongs in the key handler, and that fell out of the model rather than being
designed.** `PageDown` means "next page" in paged mode and "next screenful" in free mode. Putting
that inside `NextPage` would mean an agent could never be sure what the command would do, so the key
handler chooses between `NextPage` and `ScrollByViewports` instead. `NextPage` is now unambiguous in
every mode. This is a small thing that makes the command set much easier to reason about, and it only
became visible once commands had to be nameable from outside.

## 7b. Two bugs found driving a real document by hand

Found in the first genuinely interactive session — a 400-page, 132 MB drawing set, a window left open
for minutes, and commands arriving whenever they were typed. Both were in `Screenshotter`, and both
have one root cause: it was written for the one-shot CLI `--screenshot` flag, where *capture and exit*
is the entire purpose, and was later reused for the `capture` command without revisiting either
assumption baked into that purpose.

**The frame budget was absolute, not relative.** `warmup_frames` and `budget_frames` were compared
against the app's lifetime frame counter. For the CLI flag that is the same thing, because the request
exists from frame zero. For a `capture` command it is not: any capture arriving after about four
seconds of window uptime found `frame > budget_frames` already true, so it asked egui for an image and
declared failure in the same frame, without ever giving egui a frame to answer. The fix records the
frame the attempt started on and measures from there. The frame arithmetic moved into a pure `step`
function so it is testable without a live window, and expiry is now checked *before* sending, so a
request is never issued on the frame the attempt is abandoned.

**Resolving a capture closed the window.** Correct for the CLI flag, where a stranded window would
hang the command; wrong for a command, where the controlling process expects to keep driving.
`ScreenshotRequest` now carries `exit_when_done`, so the two callers state which they mean.

The interesting part is why M10 could not see either one. Every capture assertion in `control.rs`
captured *last* in its test, and within the first seconds of process life. That satisfies the absolute
budget by accident, and makes the process exiting afterwards indistinguishable from normal teardown —
the test then read a PNG that did exist and passed. The lesson is not "write more tests" but that a
test which exercises a step only in final position cannot see whether that step is destructive. The
new test captures, navigates, asserts the reply, and captures again, comparing the two PNGs for
inequality so a stale image cannot pass either.

This is the second time interactive use found something the suite could not — the BOM rejection was
the first. Both were at the seam between the program and the thing driving it, which is exactly where
a suite that supplies its own inputs is weakest.

## 7c. One-based page numbers everywhere visible

The same session turned up an inconsistency this plan never noticed: `go_to_page` counted from 0
while the CLI's `--start-page` and `render --page` counted from 1, and `info` printed a half-open
index range — "pages 0..1 would rasterize" — to a person. The status bar was already adding one by
hand. Three conventions in one program.

Page numbers are now one-based everywhere a person or an agent can see one, and the conversion is
carried by a type rather than by discipline. `PageNumber` wraps a `NonZeroUsize`; indices stay plain
`usize`; `PageNumber::index` and `PageNumber::from_index` are the only way across.

Three things fell out of using a type rather than a convention:

- **`{"page":0}` is refused by deserialization itself.** No hand-written guard exists to forget,
  because zero is unrepresentable. The decoder needed no change at all.
- **`visible_pages: Range<usize>` became `first_visible_page` and `last_visible_page`, inclusive.**
  A half-open range whose start counts from 1 is an invitation to an off-by-one:
  `{"start":51,"end":53}` takes a moment to read where *first 51, last 52* does not. They are also
  `Option`, so an empty document reports `null` rather than claiming page 1 is on screen.
- **The round-trip test was convention-agnostic and so proved nothing here.** `go_to_page(N)` then
  reading `current_page == N` holds under *either* numbering. The tests now also assert the scroll
  offset, which does not.

`View::current_page()` and `View::visible_pages()` deliberately still return indices: their callers
reach into the layout and the cache with them. The distinction is that a snapshot is *read* and an
index is *used*.

## 7d. A decode failure has to keep its request id

Adding the `page: 0` case to the end-to-end tests exposed something worse than the case itself. The
test hung with the window open until it was closed by hand, and the reason was two lines of
`serve_control`:

```rust
// A line we could not decode has no id to reply against, so the
// best we can do is say what was wrong.
let reply = Reply::failed(None, error);
```

That comment is true for a line that is not JSON, is not an object, or whose `id` is itself
malformed. It is false for `{"id":7,"command":"go_to_page","page":0}`, where the id parsed perfectly
and only an argument was bad. Every argument error, unknown command and missing field was answered
with `id: null`, so a client waiting for a reply to id 7 waited forever. Section 4 chose to echo `id`
back precisely so a client could correlate replies, and the error path quietly opted out of it.

`decode` now returns a `DecodeFailure { id, reason }`. The id is read before anything that can fail
with one, travels with the failure, and is `None` only when it genuinely could not be read. Two tests
pin both halves: the id survives every post-id failure, and is absent for the three cases where
claiming one would be inventing it.

Two things worth drawing out. First, this was found by *adding a test*, not by running one — the
hang was in the test's own `reply_to`, which is what a real client's wait loop looks like. Second,
§4's reasoning was sound and the implementation silently contradicted it in a branch nobody
re-read; the fix is a type that makes the id impossible to drop rather than a comment asking
future readers to remember.

## 7e. Horizontal panning, and the unit bug it uncovered

The scroll area was `ScrollArea::vertical()`, so zooming past fit-width on a 1224 pt landscape
drawing put the right-hand side of the sheet permanently out of reach — clipped, with no scrollbar
and no key to get there. Reported from a screenshot after a related fix moved the vertical scrollbar
to the window edge.

Adding it meant adding *commands*, not a gesture. A pan reachable only by mouse would be precisely
the click-only feature §2 exists to make unrepresentable. So `PanTo`/`PanBy` join the command set,
`ViewState` gains `scroll_left_pt` and `requested_scroll_left_pt` with the same
requested-versus-actual split as the vertical axis, and the snapshot reports both plus
`max_scroll_left_pt`. The coverage enforcement did its job immediately: the library compiled fine and
the *tests* refused to, because `ViewCommand::ALL`'s exhaustive match and the
every-command-has-a-behaviour-test map both failed on the two new variants.

Separate variants rather than an `axis` field on `ScrollTo`, because that enforcement works on
variants — an axis field would have been a dimension nothing forced a test to cover.

### The real find: pixels were being used as points

`Viewport`'s fields were named `width_pt`/`height_pt` and held **egui points**, which are screen
units. Everything else in the crate means **PDF points**, 1/72 inch. Two different things with the
same name, and they are equal only at zoom 1.0.

Two places consumed them as PDF points:

- `max_scroll_pt` = `content_height_pt - viewport.height()`
- `visible_pages(scroll_top_pt, viewport.height())`

Both wrong at every zoom but 1.0. Measured on a real document at fit-width, zoom 0.8366: the viewer
reported `max_scroll_pt` 7304 where the truth is 7162.6, and reported one page on screen where two
were visible — so a page a person could see was never requested and showed as a placeholder.

Why it survived: the default test viewport is 612 px against 612 pt pages, so fit-width computes
*exactly* 1.0 and every existing assertion was taken at the one zoom where the bug is invisible. Two
tests now pin it at other zooms.

`Viewport`'s fields are `width_px`/`height_px`, `View::visible_height_pt`/`visible_width_pt` divide
by zoom, and the snapshot's `viewport_*_pt` fields became `viewport_*_px`. This is the third bug in
this family — after `force_scroll` in §7a and this one's two consequences — which is enough to treat
"pixels and points are both called points" as the sharpest edge in the codebase rather than a passing
inconvenience.

`ScrollByViewports` was silently affected too: a "screenful" was a pixel count, so at 2x it advanced
twice as far as a screen.

## 8. Open decisions

1. **Does an agent get a window at all?** Still open, and now with a concrete cost attached: because
   `porpoise serve` opens a window, M10 needs a display *and* a GPU adapter wgpu will accept. The
   Windows CI runner supplies both. On Linux, xvfb gives a display but not an adapter — that needs a
   software Vulkan or GL stack (lavapipe / llvmpipe) which is not wired up, so the end-to-end job
   runs on Windows only. A headless mode would remove that asymmetry as a side effect. The tests skip
   themselves loudly rather than silently when `PORPOISE_E2E` is unset, so a headless `cargo test`
   neither fails nor pretends to have covered this.
2. **How much history does the event stream keep?** An agent that connects late, or reads slowly,
   will miss events. A small ring buffer replayed on connect is the usual answer. Needs a decision
   before M9, not before M7.
3. **Should `Capture` be able to write anywhere?** It is the one command that writes to the
   filesystem. Restricting it to a directory named at startup is more defensible; leaving it open is
   more useful. Leaning: open, since the controller already has the user's write access, but say so
   in the docs rather than leaving it implied.
4. **Coverage measurement.** The exhaustive-match trick covers the command surface, which is the
   part that matters. Whether to also wire up `cargo-llvm-cov` for a global number is a separate
   question — a percentage is easy to game and easy to satisfy without testing anything important.

## 9. Known unknowns

- **Whether egui's scroll area will cooperate.** §2 commits to letting egui own the live offset
  precisely because the alternative risks the scroll feel. If reconciliation turns out to fight
  egui's smoothing anyway, the fallback is taking full ownership and reimplementing inertia, which
  is a much larger job than M7 looks like. This is the schedule risk in phase 1.
- **Frame-rate coupling.** Commands take effect on the next frame. For an agent issuing a hundred
  commands that is a hundred frames, or about 1.7 seconds at 60 fps. If that becomes a problem the
  answer is applying a batch of commands per frame, which the model already permits but which needs
  care around commands that depend on the previous one's derived result.
- **How an agent should express "the text I can see."** Nothing in Goal 2 requires text extraction,
  but the moment search or selection lands, the addressing scheme from §1 has to be designed
  properly. hayro has no text-extraction API (`goal-1-plan.md` §5), so this is a Goal 4 problem that
  Goal 2's constraint will shape.
- **The ~150 ms frame outlier is still unexplained** (`goal-1-plan.md` §6b). It is not our code, and
  it does not block any of this — but an agent that waits on `Idle` and measures timings will see it,
  so it may finally get diagnosed by something other than a human noticing a hitch.

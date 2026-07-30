# Goal 4 — Reorganize pages and save

Status: **complete**. Milestones M14–M18 below.

This is the first goal that writes to disk. Everything before it could be wrong and cost you nothing;
this one can cost you a document. That shapes most of the decisions here.

## 1. Scope

**In:** move a page to a different position, delete a page, undo, save over the original, save as a
new file.

**Out, deliberately:**

- **Rotating pages.** Changes a page's contents rather than the document's order. Natural next step.
- **Inserting pages from another file**, extracting a range, duplicating a page. Each needs a second
  document open, which the viewer has no concept of.
- **Editing page contents** — text, annotations, form fields. A much larger problem.
- **Incremental save** (appending changes rather than rewriting). Faster on huge files, but it makes
  a correct result harder to verify, and correctness matters more here than speed.
- ~~A thumbnail grid to drag pages around in.~~ Added after the rest landed; see §7.

## 2. Two libraries, one document

We open and rasterize with `hayro`. Nothing in `hayro` writes PDFs, so saving uses `lopdf` — which
means the file is parsed twice, by two parsers, and they have to agree on what "page 3" is. If they
disagree, moving page 3 moves the wrong page and the mistake is written to disk.

Measured on the three real drawing sets in `test-pdfs/` before planning around it:

| Document | hayro | lopdf | Page tree |
|---|---|---|---|
| ROLT14_GDOT-U_6.pdf | 10 | 10 | 10 leaf kids, flat |
| Salem 6.14 PD 062226.pdf | 28 | 28 | 28 leaf kids, flat |
| WC9.19.P1_WestChicago.pdf | 400 | 400 | 400 leaf kids, flat |

Both agree everywhere, and every tree is flat. So:

- **The save path checks agreement and refuses if it fails.** Cheap, and "this document cannot be
  edited safely" is a far better outcome than a silently scrambled file.
- **A nested page tree is refused too, for now.** In a nested tree, pages can inherit `/Resources`,
  `/MediaBox`, `/Rotate` and `/CropBox` from the branch above them. Reordering across branches
  changes what a page inherits, and `lopdf` 0.44 has no inherited-attribute support — so the correct
  fix is to push inherited attributes down onto each page before flattening. That is real work, none
  of the documents at hand need it, and guessing would risk exactly the kind of silent damage this
  section exists to prevent.

`lopdf` is added with `default-features = false`. The defaults pull in `rayon` — which this workspace
removed on purpose — plus `chrono`, `jiff` and `time` for metadata we never read.

## 3. An edit is a permutation, not a mutation

The document on disk is never touched until you save. The edit is held as a list of **source page
indices in display order**: `[0, 1, 2, …]` to begin with. Moving a page reorders the list; deleting
one removes an entry. `PageOrder` in `porpoise-doc` owns this, and it is pure arithmetic over a
`Vec<usize>` — no PDF, no window, no `lopdf`.

Two things follow, both good:

- **Undo is a snapshot, not an inverse.** Push a copy of the list before each edit. For 400 pages
  that is 3.2 KB, so the simple thing is affordable, and an inverse-command stack has nothing to
  offer over it except ways to be subtly wrong.
- **A reorder invalidates no rendered pages.** Page textures stay keyed by *source* page, so moving
  page 300 to the front costs nothing to redraw.

### The hazard: two kinds of page number, again

There is now a **display position** and a **source page**, they are both `usize`, and they differ
after any edit. This project has been bitten by exactly this shape three times — pixels versus PDF
points, zero-based indices versus one-based numbers, and screen units versus document units.

Mitigation: every crossing goes through `PageOrder::source_of(position)`, the way every one-based
crossing goes through `PageNumber::index`. Variables are named `position` or `source`, never `page`.
This is a weaker guarantee than a distinct type — worth revisiting if it bites.

## 4. Saving without losing the original

A save must produce a complete correct file or leave what was there untouched. Never a truncated PDF
where a working one used to be.

- Write to a temporary file beside the destination, then rename it into place. A rename within one
  directory is atomic, so a crash mid-write leaves the original intact and a stray temp file.
- **Save As refuses to overwrite an existing file.** Overwriting is what Save is for; a Save As that
  silently replaces something is how people lose work.
- Saving is refused when the page order is unchanged *and* the destination is the source — there is
  nothing to write.

## 5. Milestones

| | | |
|---|---|---|
| **M14** | `PageOrder`: the permutation, undo, and its tests | ✅ |
| **M15** | Writing a reordered PDF, proved by rendering it back | ✅ |
| **M16** | Commands: move, delete, undo, save, save as | ✅ |
| **M17** | The viewer shows the edited order | ✅ |
| **M18** | Controls a person can actually use | ✅ |

**M14.** Pure logic in `porpoise-doc`. Move, delete, undo, and the invariants: a position is always a
valid source page, the last page cannot be deleted (a PDF with no pages is not a PDF), and undo
past the beginning is a no-op rather than an error.

**M15.** `save_reordered` in `porpoise-doc`. Load with `lopdf`, check the page count against what
`hayro` reported, check the tree is flat, permute the root `/Kids`, fix `/Count` and each `/Parent`,
write atomically. The test that matters renders the saved file back with `hayro` and compares pixels
against the original pages — reversing a document and checking that new page 1 is old page 3 is the
only way to know the bytes mean what we intended.

**M16.** `MovePage`, `DeletePage`, `Undo`, `Save`, `SaveAs` join the command set, so every edit is
reachable by an agent from the start rather than being retrofitted — the ordering benefit Goal 2's
plan predicted. The snapshot reports whether there are unsaved changes.

**M17.** `OpenDocument` holds a `PageOrder`; the scroll layout is built from the reordered geometry;
render requests and cache lookups use source pages.

**M18.** Move-up, move-down, delete, undo and save on the toolbar and on keys, acting on the current
page. Enough to reorganise a document without a thumbnail grid.

## 5a. Measured on the real documents

Reversing each drawing set in `test-pdfs/`, then rasterizing the saved file and comparing its first
page against the original's last:

| Document | Pages | Save | Size | First page is the old last page |
|---|---|---|---|---|
| ROLT14_GDOT-U_6.pdf | 10 | 33 ms | 11 → 11 MB | yes |
| Salem 6.14 PD 062226.pdf | 28 | 55 ms | 13 → 14 MB | yes |
| WC9.19.P1_WestChicago.pdf | 400 | 1.04 s | 126 → 130 MB | yes |

Two things worth knowing. **A second is a long time to block a frame**, so saving a large document
has to happen off the UI thread — the same treatment rendering and the file dialog already get.

And **the file grows by about 3%**. `lopdf` rewrites every object with its own choices rather than
preserving the original's, so this is expected rather than a leak. It does mean a save is not
byte-identical even when the order is unchanged, which is a further reason saving an unedited
document is refused rather than being a harmless no-op.

## 5b. What building it changed

**The cache was being pruned by the wrong kind of page number.** `retain_pages` kept textures whose
key fell inside the visible *display* range — but the cache is keyed by *source* page. Before any
edit the two are identical, so it was correct until the first reorder, at which point it would evict
pages that are on screen and keep ones that are not. Caught while reading the paint loop, not by a
test, and it is exactly the hazard §3 predicted: the fourth appearance in this project of two
similar numbers that must not be confused.

**`idle` is an edge, not a level, and that surprised the test I wrote.** The end-to-end test waited
for `idle` after a `move_page` and hung for a minute. An edit takes effect in the frame that accepts
it, and if it needs no new rasterization the program never leaves the settled state — so there is no
falling edge and therefore no new `idle` event. The event is emitted on the *transition*. Clients
should treat the snapshot's `idle` field as the level and the event as a notification, and wait on
`pages_reordered` after an edit. Worth stating plainly because it is a natural thing to get wrong.

**The arrow glyphs did not render.** `↑` and `↓` are missing from egui's bundled fonts and appeared
as empty boxes in the toolbar. Found by looking at a capture of the real window; no test would have.
They are the words *Up* and *Down* now.

**Verified by hand on a real drawing set.** Moving sheet 10 of the GDOT plan to the front and saving
over a copy: the window then showed *Traffic Control Plan (Shoulder Work), TCP-3* as page 1 of 10,
the status bar said *unsaved changes* until the save finished, and the saved file reopened with the
new order.

## 7. The page grid

Moving one page at a time from the toolbar is fine for a small fix and tedious for a real reshuffle,
so `Ctrl+T` opens a grid of thumbnails you can drag pages around in.

**It reuses the render pipeline rather than adding one.** A thumbnail is a page at a small zoom, so it
goes through the same worker pool and the same texture cache, keyed at its own rung. Nothing new to
keep correct, nothing new to budget, and virtualization comes free — `show_rows` only calls back for
the rows on screen, so a 400-page document costs about twenty tiny renders rather than four hundred.
Measured on the 10-page GDOT set: 18 cached rasterizations for 10 pages, which is the thumbnails plus
the main view's pages coexisting exactly as intended.

That coexistence turned out to rest on luck. `PageCache::retain_bucket` dropped every rung of a page
except one, with a doc comment saying it ran "once the wanted rung has arrived". **Nothing ever called
it.** Had anything called it, the grid and the main view would have evicted each other's textures
continuously. Removed, and the test that covered it replaced by one asserting the opposite property —
that a page may hold several rungs at once — since that is what the grid depends on.

**Dragging is a gesture; the panel is a command.** Dropping page 7 onto slot 2 produces
`MovePage { from: 7, to: 2 }`, which already existed, so the grid adds a way to *author* an edit and
no new capability — the same reasoning that keeps the file dialog off the protocol. Whether the panel
is *showing* is different: it changes what is on screen, and unlike a modal an agent that opens it can
also close it, so there is no state it can enter and not leave. Hence `set_thumbnails`, reported in
the snapshot.

**What is not tested:** the drag itself. egui's drag-and-drop cannot be driven from the control
channel, so "picking up a thumbnail and dropping it two slots over" has no automated test. The seam is
placed so that everything either side is covered — the grid's arithmetic has unit tests, `MovePage` has
end-to-end tests, and `set_thumbnails` has one — but the gesture connecting them can only be checked by
a person.

Confirmed by hand: pages land where you expect when you let go. That is the whole of the evidence for
this one, and it will stay that way unless a way to inject synthetic pointer events arrives — worth
remembering if the drop behaviour is ever changed, because nothing will fail if it breaks.

## 8. Asking before unsaved changes are lost

Closed the largest gap this goal left. Reorder twenty pages, click the X, and they were gone — no
prompt, no message. The program *knew*: the status bar said *unsaved changes*. Nothing acted on it.

### First, "unsaved changes" had to mean it

`PageOrder::is_unedited` compared the order against the document **as first opened**, and nothing ever
moved that baseline. So a saved document went on claiming changes forever: the status bar nagged and the
Save button stayed lit even though the file matched exactly.

Survivable while it only lit a status bar. Not survivable underneath a warning — **a warning that fires
when nothing is at risk is one people learn to click straight through**, which would have made the
feature worse than useless. So `PageOrder` now holds the order as it stands in the file, and
`mark_saved` moves it when a save reports success.

`mark_saved` takes **the order that was written**, not the current one. A save runs off the UI thread
and takes about a second on 400 pages, so there is a real window in which pages get moved again;
marking those as saved would tell somebody their work is on disk when only the earlier version is.
Passing the written order back from the saver makes that case come out right on its own.

A save also re-points the document at the file just written, so **Save As switches you to the new
file** — what every editor does, and what stops the nagging after a Save As.

### The guard is on the command, not the gesture

Everywhere else this project keeps dialogs off the command surface, because a box only a person can
dismiss is a box an agent can get stuck behind (`goal-3-plan.md` §1). The obvious reading here would be
to raise the question from the X button, from `Ctrl+O` and from a file drop.

That reading is wrong twice over. It repeats the check three times, so a fourth producer arrives
unguarded. And it would make the **most safety-critical behaviour in the program the only one with no
automated test**, because nothing can press an X.

So the guard sits in front of dispatch, and an agent gets the same protection a person does — which is
not a special case, since an agent that reorders pages and then opens another file loses work exactly
as a person would. The way out is `answer`, a real command with `save`, `discard` and `cancel`. Five
end-to-end tests cover the flow as a result, including the ordering that is easy to get wrong: answering
*save* must wait for the file to land before carrying the request out, not fire both and hope.

The X button reaches the same place. A close request is intercepted and turned into `Quit`, so the
window button, `Alt+F4`, the taskbar and an agent all take one path.

**`ViewportCommand::Close` comes back round as a close request.** egui-winit pushes it onto the same
event queue the X button feeds, so an interception that cannot tell "somebody clicked X" from "we asked
to close" re-raises the question forever and the window can never be shut. A `quitting` flag is what
stops that. Found by reading egui 0.35's source before writing the code rather than by watching a window
refuse to die.

### What is not guarded

- **The control channel hanging up.** There is nobody left to ask, so the window closes and the edit is
  lost.
- **A minimised window.** `intercept_close` runs in the pre-pass, which always runs, but the box is
  drawn in `ui`, which does not run while minimised. Closing from the taskbar therefore raises a
  question that is invisible until the window is restored — the same shape as the minimised-window
  behaviour recorded in `viewer.rs`.

Verified against the real window by posting `WM_CLOSE`, which is exactly what the X sends: with a clean
document it exits 0 as before, and with a reordered one it stays up reporting `awaiting_answer: "quit"`
and `idle: false`, then exits on `discard`.

## 9. Structural review

Four checks after the work above landed: does everything go through a command, is the folder structure
right, is anything too big, and is anything written twice.

**Commands: yes, and this goal closed the last hole.** Checked by mapping every write to viewer state
rather than by reading around. `state` and `open` are written in two functions, both reachable only from
`carry_out`; `porpoise_view::apply` has exactly two call sites, both inside dispatch; the page order moves
only through `edit`. Twelve producers all funnel through `dispatch`. Before §8, the X button sent
`ViewportCommand::Close` directly — the last input path that changed the program without a command.

Two exceptions remain on purpose: the file dialog (`goal-3-plan.md` §1), and dragging the scrollbar, where
egui owns the live offset and the view *reports* it back. The capability exists as `scroll_to`; only the
gesture skips dispatch.

There are now **three** exhaustive matches over `Command` — naming it, advertising it, and deciding whether
it discards unsaved work. That triplication is deliberate: each answers a different question and each fails
to compile when a command is added. Enforced repetition is not the kind DRY is about.

**Folders: nothing to change.** Dependencies run one way, tests live in the crate they test.

**One file was too big.** `viewer.rs` held 1300 lines of code and no tests, while every other large file in
the workspace is mostly tests. Two seams were clean — the toolbar and status bar, and the two overlays —
and both moved to `chrome.rs`, taking it to about 1000.

Worth stating plainly: **that bought navigability, not coverage.** Those functions need a live
`egui::Context` either way, so none of them gained a test by moving. Further splitting was considered and
declined — the render pipeline and `draw_pages` are genuinely coupled to the frame loop, and cutting them
apart to satisfy a line count would make the code harder to follow, not easier.

**The repetition that mattered was a behaviour difference.** The keyboard and the toolbar each worked out
which page edits were possible, and they had already drifted: `Ctrl+S` produced a `Save` unconditionally
while the Save button was disabled during a save. So pressing the key twice on a large document put *"a
save is already running"* in the status bar, which the button could never do. Measured on the 400-page set,
the second `save` comes back `ok:false`.

Cosmetic in itself — the message clears when the save lands — but two answers to one question is how they
drift further. Both now read `edits::Edits::available`, which takes plain values and is therefore the first
part of the toolbar's behaviour with unit tests at all. Nine of them.

Three smaller ones fixed: the filename-or-whole-path fallback written three times (now `label.rs`), two
near-identical blocks reading paths out of egui's file input, and `begin_save` asking "already saving?"
twice with the same message. That last one needed a decision rather than a deletion — with a save running
*and* nothing to write, "nothing to write" now wins, because it is the more accurate of the two and the one
that does not put an error on screen.

## 6. Known gaps

- **Nested page trees are refused** (§2). The fix is inherited-attribute push-down.
- **Whole-file rewrite on every save**, so saving a 132 MB document rewrites 132 MB.
- **The window title never changes.** Set once at startup, so it is stale after any `open` — including
  the one a Save As now implies.

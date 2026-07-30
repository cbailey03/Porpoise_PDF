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
- **A thumbnail grid to drag pages around in.** This is the honest gap: reorganising pages really
  wants one. See §6.

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

## 6. Known gaps

- **No thumbnail grid.** Moving the current page one position at a time is workable for small
  changes and tedious for large ones. A grid you can drag pages around in is the real answer and is
  its own piece of work.
- **Nested page trees are refused** (§2). The fix is inherited-attribute push-down.
- **No warning when closing with unsaved changes.** The snapshot knows, but nothing asks.
- **Whole-file rewrite on every save**, so saving a 132 MB document rewrites 132 MB.

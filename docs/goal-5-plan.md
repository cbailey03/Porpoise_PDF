# Goal 5 — Merge PDFs

Status: M19–M30 **complete**. Goal 5 is done — see §10.

Goal 4 named this on its way out the door: "**Inserting pages from another file**...
needs a second document open, which the viewer has no concept of" (`goal-4-plan.md`
§1). This goal gives the viewer that concept. Everything else it built — `PageOrder`
as a permutation, the page grid, atomic saving — was designed around exactly one
source document, so the question this plan has to answer is not "how do we bolt a
merge button on" but "what changes when a page can come from more than one place."

## 1. Scope

**In:** open a second PDF while one is already open, and add its pages to the one
being edited. The new pages land in the page grid as ordinary entries — movable,
deletable, selectable — using the reorder tools Goal 4 already built. Saving writes
one file containing pages that used to live in two.

**Out, deliberately:**

- **Choosing which pages of the second file to bring in, at insert time.** Every
  page of the inserted file comes in; unwanted ones are deleted afterward with the
  delete tool that already exists. Building a second way to pick pages, when one is
  sitting right there in the grid, is exactly the kind of duplication this project
  keeps cutting out — see the thumbnail grid's own "reuses the same pipeline" story
  in `goal-4-plan.md` §7.
- **Choosing exactly where the inserted pages land.** They are appended after the
  last page. Dropping precisely between two thumbnails would need the grid to hit-test
  a screen position against a specific cell across a native OS drag, which nothing in
  the program does today — see §6. Move them afterward with the tools that exist.
- **Bookmarks, outlines, form fields, links, and attachments.** Carried across only
  by accident, not by design; see §5 for exactly what a merge preserves and why.
- **Encrypted or password-protected source files.** `Document::open` does not handle
  encryption today, merged or not.
- **A standalone "merge these N files without opening a viewer" batch mode**, beyond
  the CLI wrapper noted in §6 as a cheap addition once the underlying primitive
  exists. This is a viewer feature, not a file-conversion utility — see §2.

## 2. Why this is a viewer feature, not a batch tool

The other way to read "merge PDFs" is a command that takes a list of files and
writes one output, with no editing session in between — closer to a Unix pipe than
to the page grid. It is a smaller change: no new render-pipeline concept, no new
command guarded or not, just a function in `porpoise-doc` and a CLI subcommand next
to `info` and `render`.

Rejected as the *primary* design, for two reasons. First, Goal 4 already recorded
what the intended shape was — "a second document open" — which is the grid-integrated
reading, not the batch one. Second, a batch tool throws away the machinery Goal 4
just finished building: the moment two files are merged into one, picking which pages
survive and where they end up is exactly what the page grid's search, multi-select,
and drag already do. A batch-only merge would need its own page-selection UI to be
useful for anything beyond "glue two files together whole," and building that UI a
second time next to a grid that already does it is the duplication this project has
spent Goal 4 removing, not adding back.

The batch primitive falls out of this design for free, though — §5's save path takes
a list of sources and an order regardless of whether that order came from a live grid
or was typed on a command line. §6 notes it as a thin, optional CLI wrapper once §5
exists, the same way `render` and `info` are thin wrappers over the same `Document`
that the viewer uses.

## 3. A page can come from more than one document

Today `PageOrder` is a `Vec<usize>` of source page indices into **one** document —
that is the entire model, and everything downstream (rendering, the cache, saving)
trusts it. Merging means a display position can now resolve to a page in *any* of
several documents, so the thing `PageOrder` hands back has to say which one.

```rust
/// A page in one of the documents contributing to what is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Source {
    /// Which contributing document, in the order it was added. `0` is the document
    /// the viewer was opened with.
    pub document: usize,
    /// A source page index within that document — what `source_of` returned alone,
    /// before there was more than one document to index into.
    pub page: usize,
}
```

`PageOrder` becomes `Vec<Source>` instead of `Vec<usize>`. `identity(page_count)`
is unchanged in effect — every entry gets `document: 0` — so every existing call
site that only ever opens one file needs no change at all. What is new:

```rust
/// Appends every page of another contributing document to the end of the order,
/// as one undo step.
pub fn append(&mut self, document: usize, page_count: usize) -> bool
```

`document` here is an index the *caller* hands in, not something `PageOrder` invents.
That is deliberate: this crate's docs already insist it knows "no PDF, no `lopdf`, no
window" (`crates/porpoise-doc/src/lib.rs`), and a document index is exactly the kind
of thing that stays true of. Which path that index names, and what `Document` backs
it, is bookkeeping the viewer owns — see §4.

`source_len: usize` — pages in the document as first opened — becomes
`source_lens: Vec<usize>`, one entry per contributing document, so the same
invariant test this module already has (`every position maps to a real source page`)
generalizes to `source.page < source_lens[source.document]` instead of comparing
against one number. `is_unedited`, `mark_saved`, and undo need no logic changes:
they already work by comparing the order to a remembered snapshot, and a `Vec<Source>`
compares the same way a `Vec<usize>` did.

### The naming hazard, extended

`goal-4-plan.md` §3 named the hazard this module already guards against: display
position and source page are both `usize`, and this project has been bitten by that
shape three times. There is now a third axis — *which document* — and the same
discipline extends to it: a variable holding one is named `document`, never `page`
or `source` on its own. `Source` bundles the two together precisely so that crossing
from a position to "the page to render" happens in one call to `source_of` and
produces one value, rather than two loose numbers that can be paired up wrong.

## 4. Rendering pages from more than one document

`OpenDocument` today owns exactly one `Arc<Document>`, one `RenderPool` built
against it, and the in-flight/failure bookkeeping keyed by `CacheKey { page, bucket }`
— all implicitly scoped to that one document. A merge needs to rasterize pages from
several `Document`s into the same cache and the same grid.

**The key has to say which document, or two different files' page 3 collide.**
`CacheKey` gains a field:

```rust
pub struct CacheKey {
    pub document: usize,
    pub page: usize,
    pub bucket: ZoomBucket,
}
```

`PageCache<T>` does not interpret its key at all — it is generic and already keyed
by a `Hash + Eq` value — so this is a mechanical, low-risk change there.
`best_for_page` (the stale-rung fallback) does need its `document` passed alongside
`page`, or a cached page 3 from one file would be offered as a fallback render for
page 3 of a different one.

**The obvious way to give the pool a second document is wrong, and worth saying
why.** The straightforward move is a second `RenderPool` per contributing document,
each sized by `RenderPool::recommended_workers()` — but that function already caps
itself at "cores minus one, clamped to four" *per pool*, so two open documents would
mean up to twice the worker threads competing for the same machine, three documents
three times, and so on. Nothing bounds it as sources accumulate over a session.

The fix is to widen `RenderPool` itself rather than multiply it: one pool, sized
once, serving jobs against a small growable list of documents.

```rust
pub struct RenderOutcome {
    pub document: usize,
    pub page_index: usize,
    // ...unchanged
}

impl RenderPool {
    pub fn submit(&self, document: usize, page_index: usize, scale: f32, tag: i64) -> bool;

    /// Registers a document the pool can be asked to rasterize from, returning the
    /// index future `submit` calls should use for it.
    pub fn add_document(&self, document: Arc<Document>) -> usize;
}
```

The worker threads currently close over one `Arc<Document>` captured at construction
(`pool.rs`'s `worker_loop`). That has to become a list every worker can read, shared
and growable at runtime — an `Arc<RwLock<Vec<Arc<Document>>>>` is the natural shape:
`add_document` takes a brief write lock to push, and a worker takes a brief read lock
per job to clone the `Arc` it needs before rendering. Contention is bounded by how
often files are inserted, which is rare compared to how often pages render, so this
is not a hot path getting a lock added to it.

With this, `RenderQueue::want` (`queue.rs`) barely changes: it already forwards
`key.page` straight to `pool.submit`; it now forwards `key.document` alongside it.
The one-decision-two-callers property `queue.rs` exists to protect — the column and
the grid must never disagree about what is worth asking for — is untouched, because
both still go through the same function.

**`OpenDocument` gains a list of contributing files, not a list of documents-with-
their-own-everything.** Only the `Arc<Document>` handles multiply; the pool, the
cache, the in-flight set, the failure map, and the zoom-rung tracking all stay
singular, because none of them are actually per-document concerns — zoom applies to
the whole viewer, and the byte budget is deliberately global (`goal-4-plan.md`
never scoped it per document either, it just never had more than one to scope):

```rust
struct OpenFile {
    path: PathBuf,
    document: Arc<Document>,
}

struct OpenDocument {
    files: Vec<OpenFile>,       // files[0] is the document the viewer was opened with
    order: PageOrder,           // now over `Source`, not a bare index
    layout: ScrollLayout,
    pool: RenderPool,           // one pool, widened per above
    cache: PageCache<...>,      // unchanged type, keyed by the wider CacheKey
    in_flight: Vec<CacheKey>,
    failures: HashMap<CacheKey, Failure>,
    submitted_bucket: ZoomBucket,
    // ...
}
```

`geometry_in_display_order`, `request_missing`, `collect_renders`, `accept_page`,
`texture_for`, and the grid's `Grid` struct all currently take one `&Document` and
resolve a bare page index against it; each of these needs to resolve a `Source`
against `files[source.document].document` instead. Mechanical in every case — the
same shape of change Goal 4's M17 already made once, going from "index" to
"`PageOrder::source_of(position)`."

## 5. Saving a document assembled from more than one file

`save_reordered` today opens **one** `lopdf::Document` from disk, keeps the pages
`PageOrder` says to keep, and writes. Saving a merge needs the retained pages from
**every** contributing file combined into one output.

lopdf 0.44 — the version this workspace already depends on — ships exactly this
recipe as `examples/merge.rs`, and it is worth reading before implementing this
milestone rather than reinventing it: `Document::renumber_objects_with(starting_id)`
shifts every object id in a document by an offset so it cannot collide with another
document's ids, `Document::max_id` says what the next free id is, and `get_pages()`
returns the page-number-to-object-id map this project's own `save.rs` already uses.
The recipe is: renumber each secondary document starting above the primary's
`max_id`, fold **all** of its objects into the primary's object table wholesale —
not only the ones on retained pages — give every retained page a `/Parent` pointing
at the one surviving page tree root, rebuild `/Kids` from `PageOrder::as_slice()`
(now resolving each `Source` to an `ObjectId` in the *right* document's page map),
and call `prune_objects()`.

That last step is why "fold in everything, then prune" is safe rather than sloppy:
`save.rs` already calls `document.prune_objects()` after every save, and lopdf's own
implementation (`processor.rs`) removes anything unreachable from the trailer by
walking references — which is precisely what drops the objects belonging to any
source page that did not make it into `PageOrder`, without this project writing a
reachability walk of its own. The same call that already exists to clean up deleted
pages (`goal-4-plan.md` §2) is what makes a merged, then-pruned file end up with no
one else's unused images and fonts left in it.

**What this does not preserve, and why.** lopdf's own merge example drops
`/Outlines` outright with a comment that they are "not supported yet." This project
inherits that limitation rather than solving it: a proper merge of two documents'
bookmarks, form fields, and named destinations is real work with its own design
questions (whose numbering wins, what a form field referencing a page that got
deleted should do), and nothing asked for it yet. Recorded here so it is a decision,
not a surprise found later.

**The existing restrictions extend rather than relax.** Goal 4 refuses to reorder a
document whose page tree is not flat, because a nested tree can mean inherited
`/Resources` or `/MediaBox` that `lopdf` 0.44 has no way to push down onto each page
first (`goal-4-plan.md` §2). That check now runs against **every** contributing
file, not only the primary — a page arriving from a nested secondary document is a
strictly worse candidate for "copy the dictionary as one object with a corrected
`/Parent`" than a page from a nested primary ever was, since it was never checked
against anything before. `PageCountMismatch` generalizes the same way: it names
*which* source disagreed with what the viewer opened, not just "the" source.

**Measure before trusting it**, the way Goal 4 measured hayro and `lopdf` agreeing
on three real drawing sets before designing around it (`goal-4-plan.md` §2). The
proof for this milestone is the same shape as Goal 4's M15: merge two of the three
files in `test-pdfs/`, save, and render the result back with `hayro` to confirm the
tail pages are pixel-identical to the second file's originals — not just "the file
opens," which a scrambled object graph can still manage.

## 6. The command, and what produces it

One new command, following the shape every prior producer question in this project
has settled on: the effect is a command, and a button, a drag, and a CLI flag are
all producers of it (`goal-2-plan.md` §1).

```rust
/// Add every page of another PDF to the end of the document that is open.
InsertFile {
    /// The file to bring in.
    path: PathBuf,
},
```

**Not guarded by `crate::confirm`.** Every intent that module guards discards or
replaces the document (`Quit`, `Close`, `Open`) — `InsertFile` only adds to it, so
there is nothing at risk to ask about. Goes in `intent_of`'s `None` arm alongside
`Save` and the page-edit commands, with a test pinning it there the same way
`saving_is_never_guarded_because_it_is_the_way_to_keep_the_changes` pins `Save`.

**Two producers.**

- A toolbar or grid button — "Add pages…" — opening the same `FilePicker` `Ctrl+O`
  already uses. No new dialog plumbing; the picker already exists precisely so a
  chosen path becomes a command through the normal route (`goal-3-plan.md` §1).
- A file dropped **onto the page grid** while a document is open. The window
  already has one drop producer, and it always means `Open` (replace) — this adds a
  second meaning that depends on *where* the drop lands, following the exact
  precedent `thumbnails_rect` and `wheel_is_for_the_pages` already set for routing a
  gesture by pointer position rather than by which widget claims it
  (`goal-4-plan.md`, `crate::input`). `drop_action` gains the grid's rectangle as an
  input and a new outcome:

  ```rust
  enum DropAction {
      Open { path: PathBuf, ignored: usize },
      Insert { path: PathBuf, ignored: usize },
      Refuse { reason: String },
  }
  ```

  Dropped **outside** the grid, or with no document open, a PDF still means `Open`
  — nothing about today's drop-to-replace behaviour changes. The hint painted while
  the file is still in the air has to say which one it is, for the same reason
  `goal-3-plan.md` §6 gives: a hint that could be wrong is worse than none.

**A thin CLI wrapper falls out for free.** §5's save path takes a list of sources
and an order; it does not care whether that order was built by dragging thumbnails
or by reading a list of file names off a command line. A `porpoise merge a.pdf
b.pdf -o out.pdf` subcommand is the same shape of thin wrapper `render` and `info`
already are over `Document` — worth adding once §5 exists, cheap enough not to need
its own milestone, and useful precisely because it needs no window: an agent that
wants two files combined does not have to open a GUI to ask for it.

## 7. Milestones

| | | |
|---|---|---|
| **M19** | `PageOrder` generalizes to `Source { document, page }`; pure, extends the existing exhaustive test suite | ✅ |
| **M20** | The render pipeline serves pages from more than one document: wider `CacheKey`, one `RenderPool` across documents, `OpenDocument` restructured | ✅ |
| **M21** | Saving combines retained pages from every contributing file into one output, proved by rendering it back | ✅ |
| **M22** | `Command::InsertFile`, wired to a button and to a drop on the grid; added to the command reference; left unguarded | ✅ |
| **M23** | End-to-end tests over the real control channel: insert, save, reopen, verify | ✅ |
| **M24** | *(stretch)* `porpoise merge` CLI subcommand, as a thin wrapper over M21 | ✅ |

**M19.** Entirely `porpoise-doc`, entirely pure — no window, no render pipeline, no
viewer changes. `Source`, `PageOrder::append`, `source_lens`, and the generalized
invariant tests. This is where §3's naming discipline either holds or does not, and
it is far cheaper to catch here than after M20 has built two crates' worth of code
on top of a `Source` that turns out to be pinned down wrong.

**M20.** The largest mechanical milestone: widen `CacheKey`, add `RenderPool::
add_document`, restructure `OpenDocument` into `files: Vec<OpenFile>`. Proof of
success is a **regression**, not a new feature: opening one document and viewing it
must behave exactly as before, since every path through this milestone still only
ever has one file open. The real test — rendering *two* files' pages at once —
waits for M22 to have a way to add a second file at all.

**M21.** The `lopdf` object-renumbering recipe from §5, generalized from Goal 4's
single-source `save_reordered` to N sources. The proof is rendering the saved file
back and comparing pixels, exactly as Goal 4's M15 did — a merged file that merely
*opens* is not evidence it merged correctly.

**M22.** The command, its two producers, and the guard decision from §6. This is
the milestone that makes M19–M21 reachable by a person or an agent rather than only
by a unit test — matching Goal 4's own M16, which did the same job for the reorder
primitives M14 built.

**M23.** Launch the real binary, drive it over a real pipe: open A, `insert_file` B,
confirm `page_count` is the sum of both, save, and reopen the saved file to confirm
the combined page count survived. Content — that the tail pages really are B's, not
just that there are enough of them — is `porpoise-render`'s job (`tests/merge.rs`
rasterizes and compares pixels); this proves the command path reaches that result,
not the bytes themselves. The same evidentiary bar Goal 2's M10 and Goal 4's M15
both insist on — a claim about what got written to disk is only real once something
reads it back.

**M24.** Optional. Only worth doing once M21 exists to wrap; if the interactive
path is judged sufficient on its own, this can slip indefinitely without blocking
anything else. Built anyway, once M21 existed to wrap — a `porpoise merge a.pdf
b.pdf -o out.pdf` subcommand, about thirty lines over `save_reordered`.

## 7a. What building it changed

**§5's `lopdf` recipe worked exactly as read from the source**, which is worth
saying plainly because it so easily could not have. `renumber_objects_with`,
`max_id`, `get_pages`, and folding a renumbered document's objects into the
primary's table wholesale, then relying on the existing `prune_objects()` call to
drop what did not make it into the final `/Kids` — every piece behaved as
`examples/merge.rs` implied it would. Measured on two real drawing sets from
`test-pdfs/` (10 and 28 pages): a correct 38-page merge in about 20 ms to insert
and 150 ms to save, growing the file by about the same ~2–3% Goal 4 measured for a
plain reorder (`goal-4-plan.md` §5a) — consistent with the file being rewritten by
a different encoder rather than damaged.

**A pre-existing bug, not one this goal introduced, got a lot easier to trip.**
`PageOrder::source_lens` (`source_len` before this goal) is fixed at open time —
or, now, at insert time — and never updated after a save. Delete a page, save,
edit again, and save a second time over the same path: the second save re-reads a
file that now has fewer pages than `source_lens` claims, and refuses with
`PageCountMismatch`. This was true of Goal 4's single-document save from the day it
shipped; nothing here changed the mechanism, only added more of it. Found while
deciding what should happen to a document's contributing files after a save, kept
out of scope because fixing it is a Goal 4 correctness question, not a Goal 5 one —
flagged separately rather than folded in silently.

**Fixed since, and it turned out to be worse than the symptom that was flagged.**
A page-count check alone cannot catch every case: two plain reorders with no
delete between them leave the count unchanged at every step, so the check never
fires — yet the second save was found to silently write the wrong pages, not
merely refuse a valid one. The fix is `PageOrder::on_disk`, a per-document record
of what a file's physical page `n` currently is in terms of the `Source` that never
changes, updated only by `mark_saved`. See `crates/porpoise-doc/src/order.rs`'s "A
source's identity outlives its file's layout" and `save.rs`'s "Saving the same path
more than once".

**The wire decoder needed its own line, and forgetting it compiled cleanly.**
`Command::shell_commands()`'s exhaustive match — the mechanism `docs/goal-2-plan.md`
§6 built specifically so an unhandled command fails to compile — covers naming and
advertising a command. It does not cover whether `protocol.rs`'s hand-written
decoder actually recognises it. `insert_file` shipped fully wired everywhere that
match reaches, and was still rejected as `"unknown command"` over the real
protocol, caught only by the end-to-end test in M23 launching the real binary. A
unit test now pins `insert_file`'s decoding specifically; the general version of
this gap — closing it the way `ViewCommand::ALL` already is, for every shell
command — is flagged as follow-up work rather than solved here.

## 8. Known gaps

- **Bookmarks, outlines, form fields, links, and named destinations are dropped**,
  not merged. §5 explains why this is inherited from `lopdf`'s own merge recipe
  rather than solved here.
- **Nested page trees are refused in every source**, extending Goal 4's existing
  restriction rather than lifting it. The fix, when it comes, is the same one Goal 4
  deferred: pushing inherited attributes down onto each page before flattening.
- **Encrypted or password-protected files are refused** — `Document::open` does not
  handle them today, and merging does not change that.
- **No way to bring in only some of a file's pages at insert time.** Deliberate; see
  §1. Delete the unwanted ones afterward.
- **No way to choose precisely where the inserted pages land.** Deliberate; see §1
  and §6. Move them afterward.
- **Whole-object-table copying, not a reachability-scoped import**, per source. Cheap
  to implement and correctness-safe because `prune_objects()` cleans up afterward
  (§5), but it does mean a large secondary file with many pages left unselected
  briefly exists in memory in full before pruning. Measured on two real drawing
  sets (§7a): merging a 10-page, 11.8 MB file with a 28-page, 14.4 MB one took
  150 ms to save and produced a 26.8 MB result — no sign of trouble at this scale,
  though neither file has pages that end up unselected, which is the case this
  point is actually about.
- **Two documents open at once means two worker pools' worth of *jobs*, served by
  one pool sized for one machine.** §4's fix bounds *thread count*; it does not
  change the fact that inserting a second 400-page document doubles the rendering
  work the same fixed pool has to get through before the grid stops showing
  placeholders. Not expected to be a problem — the grid only ever asks for pages on
  screen — but not measured yet either.

## 9. Decisions made while building it

1. **Does re-inserting the same path create a second, independent source, or reuse
   the first?** Decided as leaned: always a new one. `OpenDocument::add_file` never
   checks the incoming path against `files` already held, and its doc comment
   states why — the file may have changed on disk since it was first read.
2. **Should `InsertFile` auto-open the grid if it is not already showing?**
   Decided no, for now. The command does not touch `self.thumbnails`; the inserted
   pages land in the document and are visible in the main column immediately (they
   are appended at the end, so paged mode reaches them by paging forward, and free
   mode by scrolling) — only *picking them out individually* needs the grid open,
   which is no worse than the grid always being closed until asked for. Revisit if
   this turns out to be confusing in practice.
3. **Whether `RenderPool::recommended_workers()` still picks the right number once
   it is serving more than one document.** Left unchanged — `add_document` grows
   the document list, not the worker count, which is the whole point of §4's
   design. Not measured under real concurrent load from two large documents; still
   an open question, not a decision.

## 9a. Found while building it, out of scope for this goal

Two things surfaced during implementation that are bugs in their own right rather
than gaps in this design, and were flagged as follow-up work instead of folded in:

- ~~**A page-count check goes stale after a save.**~~ **Fixed.** Delete a page,
  save, edit again, save again over the same path, and the second save used to
  refuse with `PageCountMismatch` — or, for an edit that does not change the page
  count, silently write the wrong pages. A Goal 4 bug this goal's per-source version
  of the same check inherited rather than introduced; see §7a for the fix.
- **The wire protocol's shell-command decoder has no exhaustive-coverage check**,
  unlike `ViewCommand`'s. `insert_file` shipped fully wired everywhere except
  `protocol.rs`'s hand-written decoder, and nothing caught it until an end-to-end
  test launched the real binary. See §7a.

## 10. Extension: a two-viewport merge tab

§1 named this and deliberately left it out: "Choosing exactly where the inserted
pages land... would need the grid to hit-test a screen position against a specific
cell across a native OS drag, which nothing in the program does today." M19–M24
built the append-then-reorder path instead — insert lands everything at the end,
move it into place afterward with the tools Goal 4 already built. That is still a
correct, complete way to merge two files; this section is a second, more direct one,
asked for directly rather than inferred: a dedicated tab where dragging a page out of
a second document drops it exactly where it belongs in the first, in one gesture.

### 10.1 Scope

**In:**

- A third tab in the page grid panel, "Merge," alongside Navigation and Reorganize.
- Left viewport: the document already open, in its current display order — the
  existing single-grid rendering, unchanged.
- Right viewport: a second PDF, opened with a button or dropped directly onto that
  viewport, showing every one of *its* pages in *its own* order — a document that
  contributes nothing to `PageOrder` until something is dragged out of it.
- Multi-select in the right viewport — ctrl+click, shift+click, a marquee box,
  mirroring Reorganize mode's existing gestures exactly, just authoring a drag
  *source* instead of a drag target.
- Dragging the selection (or a single unselected page) from right to left drops it
  at the hovered position in the left viewport, landing as a contiguous block
  between two existing pages, as one undo step regardless of group size.
- The instant any of the second document's pages are placed, they are ordinary
  `Source` entries — reorderable, deletable, selectable from the *existing*
  Reorganize tab, exactly like anything `InsertFile` produces today. Nothing
  downstream needs to know a page arrived this way rather than that one.
- Placed pages stay visible and draggable in the staging viewport; nothing marks
  them "used" or prevents dragging the same page in again. A second copy of a page
  is unusual, not invalid — the project's own rule against defending against things
  that are not actually wrong applies here the same as anywhere else, and a "used"
  indicator is a cheap visual addition later if it turns out to be missed.
- A close control on the staging viewport clears it; opening a different file for
  staging simply replaces whatever was there. Neither touches anything already
  placed — those pages are independent `Source`s, backed by their own file, from the
  moment they land.
- Every one of the above reachable by command, per GOALS.md's constitution — §10.6.

**Out, deliberately, for this pass:**

- Dragging from the left viewport back into the right one. One-directional, matching
  exactly what was asked for.
- More than one staged document at a time. Staging a second file replaces the first
  rather than opening a third viewport.
- A "used" marker on a placed page in the staging viewport. See above.
- A button that inserts the current selection, as an alternative to dragging it. The
  drag is the whole affordance; a button beside it would be a second way to do the
  one thing, which is exactly the duplication §2 already argued against once.

### 10.2 Why this needs new `PageOrder` primitives, not just new UI

`append` folds a whole document's pages onto the *end*, all of them, in one call —
it has no way to express "these particular pages, wherever the user dropped them."
Composing it from what already exists — `append`, then `move_pages` to the drop
position — reaches the right final state but records **two** undo steps for a
gesture the person experienced as one: pressing undo once would leave the dropped
pages sitting at the end rather than fully gone. That is exactly the "one drag, one
undo" guarantee `move_pages` and `remove_pages` exist to keep (§3, and
`goal-4-plan.md` before it), so this needs its own primitive rather than a
composition at the call site.

A second gap: `append` does two things in one call — registers a document
(`source_lens`, `on_disk`) *and* adds every one of its pages to `order`. Staging
needs only the first half: a document known well enough to bound-check against and
to render in the staging viewport, contributing zero pages to `order` until
something is actually dragged in.

### 10.3 `PageOrder`: staging a document, then inserting some of its pages

```rust
/// Registers a document `PageOrder` can be asked to place pages from, without
/// adding any of them to the display order yet.
///
/// The bookkeeping half of `append`, split out on its own: staging a file for
/// browsing is not an edit — nothing shown changes — so this touches `source_lens`
/// and `on_disk` exactly the way `append` already does, and touches neither `order`
/// nor the undo history.
pub fn stage(&mut self, document: usize, page_count: usize) -> bool

/// Inserts `pages` of `document` — already staged, or already a contributing file
/// — as a contiguous block landing at `position`, as one undo step.
///
/// What `append` cannot express: `append` always takes every page of a document and
/// always lands at the end. This takes however many pages one drag carried and
/// lands them wherever it was dropped, mid-document or not, one page or several,
/// without ever costing more than one undo.
pub fn insert_pages(&mut self, document: usize, pages: &[usize], position: usize) -> bool
```

Unlike `append`'s "always a new index" rule (deliberate there, per §3's own doc
comment, because the file on disk may have changed since it was last read),
`insert_pages` is *meant* to be called against the same staged `document` index
repeatedly, once per drag, as different selections get pulled from the same staging
viewport. `stage` establishes what a document is, once; `insert_pages` only ever
reads that registration, never re-establishes it.

Bounds: `document < document_count()`; every one of `pages` must be
`< source_lens[document]`; `position <= order.len()`. `append` is left exactly as it
is — not reimplemented in terms of these two. The duplication between "grow
`source_lens`/`on_disk`" in `stage` and in `append` is a few lines, and the risk of
a subtle regression in a method every existing call site already depends on is not
worth trading for removing them.

### 10.4 Rendering a document's pages without an order

`Grid` in `thumbnails.rs` is built entirely around `order: &PageOrder`:
`shown_geometry`, `cell`, and `Drawn.showing` all resolve a display *position* to a
`Source` via `order.source_of`. A staged document not yet in `order` has no
positions to resolve — its pages have to be shown by iterating `0..page_count`
directly.

Two ways to get there:

(a) Generalize `Grid` to take "what to show" as an explicit `&[Source]` rather than
deriving it from `order.len()` and `order.source_of`, keeping `order` only for what
still genuinely needs it (`current`). The main grid passes `order.as_slice()`; the
staging grid passes `(0..page_count).map(|page| Source { document, page }).collect()`.
(b) A second, smaller drawing function — `draw_staging` — sharing `cell`'s geometry
and painting helpers with the main grid, but with its own interaction: multi-select
and drag-*out* only, no navigate, no move-within, no marquee-to-reorder.

(a) removes real duplication between two nearly-identical drawing loops. (b) is more
new code, but touches none of the existing, working Navigate/Reorganize path — whose
marquee-versus-drag disambiguation this file's own docs already call out as
something that took two attempts to get right. Leaning toward (b) for that reason,
but recommend deciding once both are sketched side by side rather than pre-committing
— see §10.9.

**Selection.** The staging viewport needs its own `crate::selection::Selection` —
already just a `BTreeSet<Source>` and an anchor, generic enough to reuse as-is —
separate from the one Reorganize mode uses for the main document, since a page can
be picked out in each independently and the two mean different things. The staging
grid needs only the *pick* half of `selection.rs` (ctrl+click, shift+click, marquee-
to-selection) and the drag-*out* half of `thumbnails.rs`'s `Dragged` mechanism,
generalized to whichever document the group came from — it needs none of
`PageOrder`'s move or marquee-to-*reorder* logic, because it never reorders itself.

### 10.5 The drag: a new cross-viewport payload

`Dragged(Vec<usize>)` carries display *positions* into the same order they came
from — meaningful only within one grid, one `PageOrder`. A cross-viewport drag
carries pages not yet in the target order at all, so it needs a payload that
survives the trip: which staged document, and which of its pages (necessarily the
same document for every page in one drag, since a selection lives in one viewport).

```rust
/// Pages from a document not yet in `PageOrder`, in flight during a drag out of the
/// staging viewport. Distinct from `Dragged`, which carries positions *within* an
/// order that already contains them — egui looks payloads up by type, so the two
/// can never be mistaken for each other.
struct Inserted { document: usize, pages: Vec<usize> }
```

The left grid's cell already paints an insertion bar (`paint_insertion`) on hover
during a same-order drag; recognizing an `Inserted` payload alongside a `Dragged`
one reuses that exact bar for a cross-viewport drag — the affordance being asked for
is already built, it only needs a second payload type honored where the first is
checked today. A drop produces a new `Drawn` field — `inserted: Option<(usize,
Vec<usize>, usize)>` (document, pages, landing position) — the same shape `moved`
already has, turned into the command below by the caller exactly as `moved` becomes
`MovePages`.

### 10.6 Commands

Three new effects. None guarded by `crate::confirm`: staging adds nothing to the
document, and inserting only adds to it — the same reasoning §6 already gives
`InsertFile`.

```rust
/// Opens a second document for the merge tab's staging viewport, without adding
/// any of its pages to the one being edited.
StageDocument { path: PathBuf },

/// Closes the staging viewport, forgetting whichever document was staged. Pages of
/// it already placed by `InsertPages` are unaffected — they are ordinary pages of
/// the open document by that point.
ClearStaging,

/// Inserts `pages` of the currently staged document into the open document,
/// landing as a contiguous block starting at `at`.
InsertPages { pages: Vec<PageNumber>, at: PageNumber },
```

`InsertPages` names pages of "the currently staged document" rather than taking a
path, the same way `MovePages` names positions of "the document" rather than
repeating which one is open — there is exactly one staging slot, so nothing else to
disambiguate. An agent scripting a merge without ever touching the staging viewport
already can, unchanged, via `InsertFile` (M22); this is a second, more precise way
to reach a similar effect, not a replacement for the first.

**Producers:** the drag itself; a "Stage a file…" button, reusing `FilePicker`
with a third `Purpose::Stage` — placed in the staging viewport's own placeholder
rather than on the toolbar (moved there after M28 shipped it beside "Add pages…";
staging is only ever meaningful from inside the merge tab, so the button that
starts it belongs where the pages it stages will appear); a file dropped directly
onto the staging viewport's own rectangle, a third zone for `drop_action` to
recognize alongside "onto the grid" and "elsewhere" (the exact precedent §6
already set for telling `Insert` from `Open` by drop position); and a close
control on the staging viewport for `ClearStaging`.

### 10.7 Wiring

- `GridMode` gains a third variant, `Merge`, alongside `Navigate` and `Reorganize` —
  same tab row, same `SetGridMode` command, same exhaustiveness test this file's own
  `every_mode_is_listed` already relies on to catch an unlisted variant at compile
  time.
- `thumbnails::draw` dispatches to a new `draw_merge` for `GridMode::Merge` rather
  than folding a two-viewport layout into the single-grid `draw`. The tab row and
  search box stay shared; whether the search box should narrow the staging viewport
  too, in addition to the main one, is worth deciding once both grids exist side by
  side rather than before — see M30.
- Panel width: today's `PANEL_WIDTH`/`PANEL_MIN_WIDTH`/`PANEL_MAX_WIDTH` size one
  grid up to four columns. Two side-by-side viewports need roughly double that.
  Simplest: remember the panel's width from before entering Merge mode, widen it
  (clamped to a new, wider maximum sized the same way today's constants are — from
  `THUMBNAIL_WIDTH`, not guessed) while the tab is active, and restore the
  remembered width on leaving.
- `OpenDocument` needs somewhere to hold the staged file. Whether that is a new
  `Option<OpenFile>` separate from `files` (promoted into `files` at the moment its
  first page is inserted) or registered into `files` immediately at staging time
  (since `RenderPool`/`PageOrder::stage` already tolerate a document contributing
  zero pages to `order`) is left open — see §10.9. Both produce the same externally
  visible behavior; the second reuses more of M20's existing plumbing.
- `confirm.rs`: `StageDocument`, `ClearStaging`, and `InsertPages` join `InsertFile`
  in the unguarded arm, ~~each pinned by its own test the way
  `inserting_a_file_is_never_guarded_because_it_only_adds_pages` already pins
  `InsertFile`~~. **Landed as one test covering all three**
  (`staging_and_inserting_pages_are_never_guarded`) rather than three separate
  ones — a regression that broke only one command's guard-exemption would not be
  localized by test name the way the plan intended.
- `protocol.rs`: given §9a's own finding that the hand-written decoder has no
  exhaustive-coverage check, ~~each new command's decoder arm gets a test pinning
  its wire form specifically~~ **landed the same way as `confirm.rs` above: one
  test for all three** (`the_merge_tab_commands_decode_with_their_arguments`),
  rather than assuming the general fix (still open, unstarted follow-up work)
  lands first.

### 10.8 Milestones

| | | |
|---|---|---|
| **M25** | `PageOrder::stage` and `PageOrder::insert_pages`; pure, extends the existing invariant tests | ✅ |
| **M26** | A second `Selection` instance and the `Inserted` drag payload | ✅ |
| **M27** | `draw_merge`: two viewports side by side, the cross-viewport drag, the insertion-bar hover reused from Reorganize | ✅ |
| **M28** | `StageDocument`, `ClearStaging`, `InsertPages`; unguarded; wired to the drag, a "Stage a file…" button, a drop on the staging viewport, and its close control | ✅ |
| **M29** | End-to-end tests over the real control channel: stage, insert one page mid-document, insert a multi-page selection, clear staging, save, reopen, verify | planned |
| **M30** | *(optional)* the search box narrows the staging viewport as well as the main one | ✅ |

**M25.** Entirely `porpoise-doc`, entirely pure, the same reason M19 was: cheapest
place to catch a `Source`-shaped mistake, before M26–M27 build two crates' worth of
UI on top of it. Proof is the invariant suite generalized the way M19's was —
`insert_pages` must never lose, duplicate, or invent a page, whatever `position` and
however many `pages`.

**M26.** The interaction half without the two-viewport layout yet: `Selection`
generalized so a second instance can pick from a staged document, and `Inserted`, the
payload type the main grid's hover logic will tell from `Dragged` once M27 wires up
the recognition. Provable without a window — `Selection`'s own tests already cover
picking, extended with a staged-document fixture; `Inserted`'s only real claim, that
`DragAndDrop` never mistakes it for `Dragged`, is a headless `egui::Context` test
away.

§10.4's claim that `Selection` was "already... generic enough to reuse as-is" was
half right. The *storage* was — `BTreeSet<Source>` never cared which document a
`Source` named. Every *method* was not: each took `order: &PageOrder` and called
`order.source_of(position)` to resolve one, which only works for a position that is
actually in that `PageOrder`'s display order — and a staged document contributes
none until something is dragged out of it, so there was no order to hand this module
for the staging pane at all. Fixed by taking `shown: &[Source]` instead of a
`PageOrder`: the main grid now passes `order.as_slice()`, the staging grid will pass
`(0..page_count).map(|page| Source { document, page })` once M27 builds it — the same
generalization `PageOrder::on_disk` went through for a different reason
(`docs/goal-5-plan.md` §9a), decoupling a module from a concrete `PageOrder` in favour
of the plain slice it actually needed. Every existing call site and test changed
mechanically (`&order` to `order.as_slice()`) with no behaviour change, confirmed by
the full existing suite passing unmodified in substance.

`Inserted` is introduced with nothing outside its own test constructing it yet,
because its producer — the staging viewport — is M27's, not M26's. Left as ordinary
dead code, `cargo clippy` would have caught it as `dead_code` in the non-test build
even though the test build was satisfied; addressed with `#[cfg_attr(not(test),
expect(dead_code, reason = "..."))]` rather than a plain `#[allow]`, so the exception
expires loudly — a lint warning, not a silent gap — the moment M27 gives it a real
caller and the `expect` goes unfulfilled.

**M27.** The layout milestone, and the one most worth prototyping before committing:
two viewports, sized per §10.7, with the staging one wired for pick-and-drag-out and
the main one wired to recognize `Inserted` on hover and on drop. Proof, like M20's,
is partly a regression check — Navigate and Reorganize must render exactly as before
— and partly new: a drag from the right viewport lands at the hovered position in
the left one, verified by hand the way every other pointer gesture in this file is
(`thumbnails.rs`'s own module docs name this limitation for the gestures already
shipped; a new one inherits it rather than escaping it).

§10.9's first open decision resolved as leaned: a separate `draw_staged_grid` rather
than generalizing `Grid` to an explicit `&[Source]`. `GridMode::Merge`'s cell behaviour
— click navigates, and a hover/drop recognizes `Inserted` — shares nothing with
Reorganize's click-and-drag-to-reorder, so folding both into one `cell` match arm
would have meant a fourth kind of cell logic wedged into a function already
choosing between two; a third top-level match arm plus a sibling function for the
staging pane's own (pick, drag out, marquee) kept each mode's logic legible on its
own, at the cost of the small, deliberate duplication between `draw_single_grid` and
`draw_staged_grid`'s layout arithmetic.

**A real bug found by construction, not by running anything: two grids, one memory
slot.** `marquee`'s remembered drag-origin lives in `egui::Context` memory under a
fixed `Id`. `GridMode::Merge` draws two grids — the main viewport and the staging
one — in the same frame for the first time; with the old fixed `Id` a box started in
one could have been reported as finishing in the other, since `Context` memory is
keyed by `Id` alone and nothing before now ever drew two of these in one pass to
expose it. Fixed by threading a `salt` through `marquee`/`marquee_origin`, pinned by
`different_salts_give_the_marquee_different_memory_slots`.

**`Inserted`'s temporary `#[expect(dead_code)]` (§9a) is gone**, exactly as its own
doc comment promised: `staged_cell` now constructs one on every drag start, so the
compiler would catch it going quiet again on its own.

**Scope actually built, versus what the milestone table's one-liner suggested.**
Building `draw_staged_grid` surfaced that it needs somewhere to read a document's
geometry and page count from — `Grid.staged: Option<StagedInfo<'a>>`, added this
milestone rather than M28. But `OpenDocument` has nowhere to *populate* one yet
(§10.7's second open decision, still open): `StageDocument` is M28's command, so
`grid.staged` is always `None` today and the right pane always shows its
placeholder. That is not a gap in what M27 proves — Navigate and Reorganize are
untouched, the merge tab's static layout renders correctly in a real window
(confirmed by `the_merge_tabs_two_viewports_render_without_a_staged_document`,
switching to the tab over the real control channel and capturing it), and every
pure piece (`StagedInfo`/`Selection` wiring, the salt fix, the payload distinctness)
is unit-tested. What is *not* provable yet, and could not be made so without pulling
M28 forward, is the drag itself actually landing a page — there is nothing to stage
a document from until then.

**The panel does not auto-widen when the tab opens**, a simplification from §10.7's
"remember and restore" sketch: egui only honours a panel's `default_size` the first
time it opens, so there was no cheap way to also resize one already open from here.
Widened the *allowed* range instead (`PANEL_MERGE_MIN_WIDTH`/`MAX_WIDTH`, roughly
double the single-grid ones) so the tab is at least draggable to a comfortable
width, and left auto-resize as a nice-to-have rather than blocking on it.

**M28.** The commands, their producers, and the guard decisions — the milestone that
makes M25–M27 reachable by a person or an agent, the same job M22 did for M19–M21.

§10.9's second open decision resolved as leaned: `OpenDocument` gained a
`staging: Option<usize>` pointer into `files` rather than a parallel `Option<
OpenFile>` slot. Staging now goes through the exact same `add_file` every insert
already uses — `RenderPool` registration and all — and `staging` only ever says
*which* entry is the current staging slot, never holds one itself. Replacing the
staged document points `staging` at a fresh index; the old entry is never
referenced again, the same as an inserted file's is not.

The staging viewport's own `Selection` turned out not to need a command of its
own, unlike the main grid's. `SetSelection` exists because an agent has to be able
to read and set *exactly* what a person is looking at — the keyboard's Delete acts
on whatever `selection` currently holds. Nothing analogous acts on "whatever is
selected in the staging pane": `InsertPages` already names the exact pages it
wants, so picking several out first is a mouse convenience for one drag, not a
piece of state anything else consults. Left as plain UI state, updated directly
from the grid's own `picked`/`marquee` output rather than dispatched.

Verified past the unit level: `an_agent_can_stage_a_document_and_insert_its_pages_
and_save_it` drives the real control channel — stage, insert one page mid-document
(not appended, the entire point of this over `InsertFile`), insert a second,
multi-page selection from the *same* staged document, clear staging without
disturbing what was already placed, save, and reopen to confirm the page count
survived. A screenshot taken the same way confirmed the toolbar's new **Stage a
file…** button and both live viewports render real thumbnails, not just the M27
placeholder.

All three new commands' decoder arms are pinned by one combined test
(`the_merge_tab_commands_decode_with_their_arguments`), per §9a's own finding
about `insert_file` — not deferred to the general exhaustive-coverage fix, which
is still unstarted follow-up work.

**M29.** Substantially covered by M28's own verification above — the real-pipe,
real-window proof this milestone asked for already exists. What is left, if
anything, is judged once M28's test is read back rather than assumed: additional
edge cases (a save immediately after staging with nothing yet inserted; clearing
staging mid-drag) if any turn out to matter in practice.

**M30.** Optional, and taken: narrowing the staging viewport by page number turned
out to need its own resolution of the query rather than reusing the main grid's,
which `Viewer::staged_filter`'s own doc comment argues for and
`the_search_box_narrows_the_staging_viewport_independently` proves — a 3-page
primary and a 10-page staged document searching `"1-9"` at once, where the
primary clamps to its own three pages and the staged document correctly shows
nine, not the primary's three. `Snapshot` gained `staged_filtered_pages` to match,
mirroring `filtered_pages` exactly, so an agent can read what the staging pane
shows the same way it already can for the main one. The narrowing warning
("clear the search to insert pages here") already covered `GridMode::Merge` since
M27; nothing new was needed there.

No open questions remain. Goal 5's two-viewport merge tab is complete: stage a
document, search either pane down to the pages that matter, drag a selection into
place, and save — all reachable by hand and by an agent, over the same commands.

### 10.9 Decisions made while building it

1. ~~generalize `Grid` to an explicit `&[Source]`, or a second `draw_staging`
   function?~~ **Resolved: (b), a second function.** `GridMode::Merge`'s cell
   behaviour shares nothing with Reorganize's click-and-drag-to-reorder, so a
   third top-level match arm plus a sibling function kept each mode's logic
   legible on its own. See M27's retrospective.
2. ~~where does the staged file live before its first page is placed?~~
   **Resolved: straight into `files`, via a new `staging: Option<usize>` pointer.**
   Staging reuses `add_file` exactly as `InsertFile` does; `staging` only ever
   says which entry is the current slot. See M28's retrospective.
3. **M30**, deferred rather than decided against.

### 10.10 Hardening found by a review after "done"

A review across the backend, the app's state layer, the UI, and the tests turned
up one real correctness gap the milestones above did not catch: staging a
document registers it with `PageOrder` permanently — `stage` has no `unstage`,
by the same "never reuse a document index" rule `append` already follows — but
`save_reordered` was loading, renumbering, and flatness-checking *every*
registered document on *every* save, including ones that had only ever been
staged and never had a page inserted. Stage a file to look at it, decide not to
merge from it, clear staging — and every save from then on silently depended on
that file still existing and still parsing, for no reason connected to what was
actually being saved.

**Fixed** in `save_reordered`: a document not named by any `Source` currently in
`order.as_slice()` is now skipped entirely rather than loaded — an empty page
table stands in for it, since nothing will ever look one up there.
`porpoise-render`'s `tests/merge.rs` gained
`a_staged_but_never_inserted_document_does_not_block_saving`, which stages a
document, deletes its file, and confirms the save still succeeds.

The same pass replaced the `assert_eq!` at the top of `save_reordered` — guarding
a mismatch between `sources` and `order.document_count()`, a caller bug the type
system cannot rule out — with a proper `SaveError`. A panic there would run on
the background thread `saver.rs` spawns for every save, and would have dropped
the result silently rather than ever reaching whoever was waiting on it.

It also found and fixed several stale doc comments this goal introduced
(`OpenDocument::order`'s "identity" claim contradicting itself over what counts
as an edit, a miscounted `OpenFile` field reference, `PageCountMismatch::opened`
still describing what a document was "opened with" after this goal moved its
real source to `PageOrder::on_disk`, and the "M23" and "each pinned by its own
test" claims corrected above) and two small duplications (`PageOrder::append`/
`stage`'s registration bookkeeping, and the layout-metrics block shared by
`draw_single_grid`/`draw_staged_grid`) — none behavior-changing, recorded here
because a plan document going stale the moment a review looks past it is the
same problem in miniature.

### 10.11 Select All, and a deliberate break from §10.6's own precedent

Added after "done": a **Select All** button in the staging viewport, next to its
close control, picking out every page of the staged document at once — the
mouse convenience for what was previously only reachable one page (or one
marquee) at a time.

Unlike `staging_selection` itself, this shipped as a real command,
`Command::SetStagedSelection { path, pages }`, decided against the option of
leaving it a click-only UI update. The reasoning §10.6 gave for
`staging_selection` staying outside the command model — "nothing else consults
what is currently selected in the staging pane" — still holds; what changed is
that a person clicking **Select All** and an agent asking for the same thing
are, this time, the same request, so the button dispatches the command rather
than writing `staging_selection` directly.

It also breaks `InsertPages`'s own precedent of naming no document at all,
because "there is exactly one staging slot, so nothing else to disambiguate."
`SetStagedSelection` takes `path` anyway: staging more than one document at a
time is explicitly on the table for later, and a command whose shape already
names *which* staged document it means keeps meaning the same thing once there
is more than one to choose from — where `InsertPages`'s implicit "the staged
document" would need to change shape or grow ambiguous. Today, with exactly one
slot, `path` is validated against it and a mismatch is refused rather than
silently applied to whatever happens to be staged — which also catches a stale
request left over from before a `clear_staging`/`stage_document` swap.

`Snapshot` gained `staged_selection`, mirroring `selection`, so an agent can
read back what **Select All** — or its own `set_staged_selection` call — left
picked out. Verified past the unit level by
`an_agent_can_select_all_of_the_staged_documents_pages`: stage, refuse a
mismatched path, select all three pages, confirm `unchanged` on repeating it,
clear the selection with an empty list, and confirm `nothing is staged` once
`clear_staging` runs. A screenshot taken the same way as every other milestone
here confirmed the button's placement and the selection highlight rendering
across all three staged pages.

### 10.12 Multiple simultaneous stages

§10.11's closing paragraph predicted this: staging more than one document at
once was "explicitly on the table for later." This is that later.

**What changed.** `OpenDocument.staging` went from `Option<usize>` — one
pointer, replaced by every new `stage_document` — to `Vec<Staged>`, one entry
per currently staged document, each carrying a new `StageId`: a small,
permanent, 1-based label (mirroring `PageNumber`'s validated-newtype pattern,
minus an `index()` — it is never used to index anything) assigned by a
`next_stage_id` counter that only ever increases. The same "never reuse an
index" discipline `add_file` and `PageOrder::stage`/`append` already held to,
applied to a second counter: clearing a stage never lets its number mean a
different document later.

Four commands changed shape as a direct consequence. `ClearStaging` and
`InsertPages` gained a required `stage: StageId` — there is no longer a single
implicit "the staged document" to mean. `SetStagedSelection`'s `path` field,
added in §10.11 specifically so this day would not need a second migration, is
`stage` instead now: the same file can be staged twice at once (to drag
different page ranges from each, or after re-staging following an external
edit), so `path` stopped being a unique key the moment more than one slot
existed. A new `SetActiveStage { stage }` joined them, for the same reason
`SetGridMode` is a command rather than a click-only tab: switching which pane
is visible changes what is on screen, and this codebase does not have a
control an agent cannot also drive.

**What did not change.** `porpoise-doc`'s `PageOrder::stage`/`insert_pages`
already took an explicit `document: usize` and already had tests proving three
simultaneously-registered documents work — the single-slot limit turned out to
live entirely in the app layer, never in the pure logic underneath it.
`save_reordered`'s fix from §10.10 (skip a document that contributes no
retained page) already generalized to any count; only its regression test
covered exactly one, so a new sibling test in `porpoise-render/tests/merge.rs`
(`two_staged_but_never_inserted_documents_do_not_block_saving`) closes that gap
now that it matters for real. `Inserted`'s drag payload kept its plain
`document: usize` — it stays decoupled from the stage concept the same way
`StagedInfo.document` already was, and `Viewer::draw_thumbnails` is the one
place that translates it back to a `StageId`, right before a drop becomes a
command.

**The UI decision.** Two shapes were on the table: every staged pane visible
at once (a real multi-column layout, new width-budget math, N-way divider
painting, N-way drop-zone hit-testing), or one visible pane switched by a
small tab strip, addressed by `stage` regardless of which tab is showing.
Chose the tab strip — the merge tab's existing two-column layout, its width
constants, its divider and its drop zone are all completely unaffected, and
every command already names a specific stage, so an agent never has to care
which tab a person is looking at. `Grid` gained `staged_tabs: &[StagedTab]`
(id and path, deliberately narrower than `StagedInfo`, the same reasoning that
one gives for being narrower than `Grid`) alongside its existing
`staged: Option<StagedInfo>`, now understood as "the *active* stage's info."
`stage_tabs()` is its own function rather than a generalization of the
existing `GridMode`-specific `tabs()`, because that one is fixedly typed to a
closed, compile-time-checked set (`GridMode::EVERY`) and a runtime-sized list
of stages needs its own loop.

Each tab carries its own close control, so a non-active stage can be cleared
without switching to it first — the "Merge from" header's old close button was
retired in favor of it, one producer of `ClearStaging` instead of two doing
the same thing. The trailing **+** is the relocated **Stage a file…** button,
always present — even with nothing staged yet, since it is now the only way
to stage a first document — rather than conditional the way the header's
Select All still is.

**Selection and the active pane.** `Viewer.staging_selection: Selection`
became `staging_selections: HashMap<StageId, Selection>` — one entry per
stage, created lazily on the first pick or marquee in that pane, so switching
tabs never loses what was picked in another. A new
`active_stage: Option<StageId>` tracks which one the single visible pane
shows: a freshly staged document becomes active automatically (matching what
one slot always did), and clearing the active stage falls back to whichever
remaining stage was staged most recently — `StageId` only ever increases, so
the highest one left is the most recent — rather than leaving the pane blank
while others are still open. Leaving `GridMode::Merge` entirely still clears
every stage's selection at once, the same reasoning that already governs the
main grid's on leaving Reorganize; switching *tabs within* Merge deliberately
does not reach this, since a stage's own pane is never out of reach the way
the whole tab is.

**`Snapshot`.** The three singular fields (`staged: Option<String>`,
`staged_filtered_pages`, `staged_selection`) collapsed into one
`staged: Vec<StagedSnapshot>`, each entry carrying `id`, `path`, `page_count`
(a small genuinely new capability — there was previously no way to learn a
staged document's page count without a round trip), the shared query resolved
against that document specifically, and that document's own selection. A
top-level `active_stage` mirrors `grid_mode` — the same shape this codebase
already chose for "which tab is showing."

**Verified** past the unit level by
`an_agent_can_stage_and_merge_from_more_than_one_document_at_once`: stage two
documents, confirm distinct ids and that the second becomes active unasked,
insert from the non-active one first (proving addressing by id does not care
what is visible), switch the active pane, pick out different pages in each
stage's own selection, clear one and confirm the other's entry, selection and
already-placed pages are untouched and the active pane falls back rather than
going blank, then save, reopen and confirm the combined page count. A
screenshot confirmed the tab strip's own rendering and that switching tabs
actually redraws the correct document.

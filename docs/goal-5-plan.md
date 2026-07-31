# Goal 5 — Merge PDFs

Status: **complete**. Milestones M19–M24 below.

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
the tail pages are B's. The same evidentiary bar Goal 2's M10 and Goal 4's M15 both
insist on — a claim about what got written to disk is only real once something
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

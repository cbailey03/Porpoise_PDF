//! Writing a reordered — and possibly merged — document back out.
//!
//! The only part of this crate that uses `lopdf`, and the only part that writes to
//! disk. Both facts drive the shape: guards first, then a write that cannot leave a
//! half-finished file where a working one used to be.
//!
//! # Why two parsers, and what keeps them honest
//!
//! Pages are opened and rasterized with `hayro`, which cannot write PDFs, so saving
//! uses `lopdf`. Each contributing file is therefore parsed twice, once by each
//! library, and the two have to agree on what "page 3" of a given file is — if they
//! disagree, a reorder moves the wrong page and writes the mistake to disk.
//! [`save_reordered`] refuses rather than guessing, for every contributing file.
//!
//! Measured on three real drawing sets before relying on it for a single document:
//! hayro and lopdf agreed on the page count for all three, and all three had flat
//! page trees. See `docs/goal-4-plan.md` §2.
//!
//! # Merging more than one document
//!
//! [`PageOrder`] can now name pages from more than one contributing file
//! (`docs/goal-5-plan.md` §3), and [`save_reordered`] takes one source path per
//! document `order` refers to. Combining them uses the recipe `lopdf` 0.44 ships as
//! its own `examples/merge.rs`: every object of a secondary document is renumbered
//! with [`lopdf::Document::renumber_objects_with`] so its ids cannot collide with
//! the primary's, then folded into the primary's object table wholesale — not only
//! the objects on retained pages. `Document::prune_objects`, which this module
//! already calls to clean up deleted pages, is what drops everything a merge pulled
//! in but `order` did not keep; lopdf's own reachability walk does the filtering, so
//! nothing here has to.
//!
//! Bookmarks, outlines, form fields and named destinations are not carried across —
//! `lopdf`'s own merge example drops them too, with the same reasoning: merging them
//! correctly is real work with its own design questions, and nothing has asked for
//! it yet. See `docs/goal-5-plan.md` §5.
//!
//! # Saving the same path more than once
//!
//! A `Source`'s `page` is fixed at open or insert time and never changes — but a
//! save rewrites a document's file, compacted to only its retained pages and
//! renumbered from zero, so re-reading that file afterwards no longer agrees that
//! physical page `n` is `Source`'s page `n`. [`PageOrder::on_disk`] is what closes
//! that gap: it names, for each document, the stable `Source` its file's physical
//! page `n` currently is, updated only by [`PageOrder::mark_saved`]. This module
//! looks pages up through it rather than through `source.page` directly, so saving
//! the same path a second time — after a delete has changed its page count, or even
//! after a plain reorder that has not — still finds the pages `order` actually
//! means. See `docs/goal-5-plan.md` §9a, where saving twice over the same path was
//! found to either refuse a save that should succeed, or write the wrong pages
//! without complaint.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use lopdf::{Document as LoDocument, Object, ObjectId};

use crate::{PageOrder, Source};

/// Why a document could not be saved.
///
/// Every variant means "nothing was written". A refusal has to be distinguishable
/// from a partial write, because the whole point is that the original survives.
#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    /// A contributing file could not be read or parsed for writing.
    #[error("could not read {path} for saving: {detail}")]
    Source {
        /// The file we tried to read.
        path: PathBuf,
        /// The parser's complaint.
        detail: String,
    },
    /// hayro and `lopdf` disagree about how many pages one contributing file has.
    ///
    /// Refused rather than guessed. If `lopdf` sees a different set of pages than
    /// `hayro` did for this file, then the positions naming it do not mean what the
    /// person editing saw, and the result would be scrambled in a way nobody
    /// notices until later.
    #[error(
        "cannot edit {path} safely: its page tree was expected to hold {opened} page(s) \
         but reads as {found} for writing"
    )]
    PageCountMismatch {
        /// The file that disagreed.
        path: PathBuf,
        /// What [`PageOrder::on_disk`] expects this document's file to physically
        /// hold — the count as of its last save, or as first opened, inserted, or
        /// staged if it has never been saved over. Not necessarily what the viewer
        /// opened it with; see the module docs' "saving the same path more than
        /// once".
        opened: usize,
        /// What the writer found.
        found: usize,
    },
    /// A contributing file's page tree has branches, so reordering — or merging —
    /// could change what its pages inherit.
    ///
    /// In a nested tree a page can inherit `/Resources`, `/MediaBox`, `/Rotate` or
    /// `/CropBox` from the branch above it. Moving pages between branches, or into
    /// another document entirely, changes that, and `lopdf` 0.44 offers no
    /// inherited-attribute support, so doing it correctly means pushing those
    /// attributes down onto each page first. Not yet implemented; refused
    /// meanwhile, because the failure mode is a document that renders wrong rather
    /// than one that fails to open.
    #[error("cannot edit {path} yet: its page tree is nested, which this cannot reorder safely")]
    NestedPageTree {
        /// The file.
        path: PathBuf,
    },
    /// A contributing file's page tree could not be understood at all.
    #[error("could not read the page tree of {path}: {detail}")]
    PageTree {
        /// The file this is about, or the intended destination when the problem is
        /// with the combination rather than any one input.
        path: PathBuf,
        /// What was wrong.
        detail: String,
    },
    /// Writing failed.
    #[error("could not write {path}: {detail}")]
    Write {
        /// Where we tried to write.
        path: PathBuf,
        /// The underlying failure.
        detail: String,
    },
    /// The destination already exists and we were told not to replace it.
    #[error("{path} already exists")]
    WouldOverwrite {
        /// The destination.
        path: PathBuf,
    },
}

/// Whether an existing destination may be replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overwrite {
    /// Replace whatever is there. What "Save" means.
    Allow,
    /// Refuse if the destination exists. What "Save As" means — silently replacing
    /// somebody's other file is how work gets lost.
    Refuse,
}

/// Writes the documents named by `sources` to `destination`, keeping only the pages
/// `order` names and in the order it names them.
///
/// `sources` must have exactly one entry per document [`PageOrder::document_count`]
/// counts — not only ones with a page currently retained; see [`PageOrder::stage`]
/// — in the same order [`PageOrder::source_lens`] reports them: `sources[0]` is the
/// document the viewer was opened with, `sources[1]` is the first one inserted or
/// staged, and so on. Every retained page keeps the shape it had in its own file;
/// only the document each page belongs to, and the objects that page depends on,
/// move.
///
/// The write is atomic: the new document goes to a temporary file beside the
/// destination and is renamed into place. A rename within one directory either
/// happens or does not, so an interrupted save leaves every input intact and a
/// stray temp file, never a truncated PDF.
pub fn save_reordered(
    sources: &[PathBuf],
    order: &PageOrder,
    destination: &Path,
    overwrite: Overwrite,
) -> Result<(), SaveError> {
    // The type system does not see that `sources` has one entry per document
    // `order` refers to — that is the caller's responsibility, per the doc comment
    // above — so a mismatch is refused here like every other invariant this module
    // checks, rather than asserted. This runs on a background thread (see
    // `porpoise-app`'s `saver.rs`), where a panic would drop the result silently
    // instead of ever reaching whoever is waiting on the save.
    if sources.len() != order.document_count() {
        return Err(SaveError::PageTree {
            path: destination.to_path_buf(),
            detail: format!(
                "given {} source path(s) for an order spanning {} document(s)",
                sources.len(),
                order.document_count()
            ),
        });
    }

    if overwrite == Overwrite::Refuse && destination.exists() {
        return Err(SaveError::WouldOverwrite {
            path: destination.to_path_buf(),
        });
    }

    // `sources` is asserted above to have one entry per document `order` names, and
    // `PageOrder` always names at least one — but that is an invariant the type
    // system does not see, so this is a real refusal rather than an unwrap.
    let Some((primary_path, secondary_paths)) = sources.split_first() else {
        return Err(SaveError::PageTree {
            path: destination.to_path_buf(),
            detail: "no source documents given".to_owned(),
        });
    };

    let mut primary = LoDocument::load(primary_path).map_err(|error| SaveError::Source {
        path: primary_path.clone(),
        detail: error.to_string(),
    })?;
    let root = page_tree_root(&primary, primary_path)?;
    ensure_flat(&primary, root, primary_path)?;

    // One object id per stable `Source`, keyed by the `Source`s `PageOrder::on_disk`
    // says document 0's file physically holds — not by `source.page` directly, since
    // a prior save may have already renumbered that file from zero. Filled in as
    // each document is loaded and validated.
    let mut pages_by_document: Vec<HashMap<Source, ObjectId>> = vec![page_ids_by_source(
        &primary,
        primary_path,
        on_disk_pages(order, 0, primary_path)?,
    )?];

    // Documents actually named by a retained page. `stage` (`docs/goal-5-plan.md`
    // §10.7) registers a document with `order` before any of its pages are placed —
    // the merge tab does this the moment a second file is opened, whether or not
    // anything is ever dragged from it — and nothing ever un-registers one. A
    // document that stays merely staged, or was staged and then cleared, must not
    // become load-bearing for every save from then on: if it were loaded and
    // validated unconditionally below, moving, deleting or breaking a file nobody
    // ever actually merged from would stop *this* document from saving.
    let referenced: HashSet<usize> = order
        .as_slice()
        .iter()
        .map(|source| source.document)
        .collect();

    // Every retained page has to hang off `root` in one merged object table.
    // Secondary documents are folded in wholesale — not only their retained pages —
    // because `lopdf` gives us no cheaper way to know what a page's `/Resources`
    // transitively reaches, and `prune_objects` below drops what does not survive
    // into the final tree anyway. See the module docs and `docs/goal-5-plan.md` §5.
    let mut next_id = primary.max_id + 1;
    for (offset, path) in secondary_paths.iter().enumerate() {
        let document = offset + 1;
        if !referenced.contains(&document) {
            // Nothing above will ever look this document up — no position in
            // `order` names it — so an empty table is exactly as good as the real
            // one, and costs nothing to be wrong about.
            pages_by_document.push(HashMap::new());
            continue;
        }
        let mut secondary = LoDocument::load(path).map_err(|error| SaveError::Source {
            path: path.clone(),
            detail: error.to_string(),
        })?;

        // Renumbered before anything reads an id out of it, so every id this
        // function sees from here on — its root, its pages, its cross-references —
        // is already the one it will carry in the merged file.
        secondary.renumber_objects_with(next_id);

        let secondary_root = page_tree_root(&secondary, path)?;
        ensure_flat(&secondary, secondary_root, path)?;
        let ids = page_ids_by_source(&secondary, path, on_disk_pages(order, document, path)?)?;

        next_id = secondary.max_id + 1;
        primary.max_id = primary.max_id.max(secondary.max_id);
        primary.objects.extend(secondary.objects);
        pages_by_document.push(ids);
    }

    let mut kids = Vec::with_capacity(order.len());
    for position in 0..order.len() {
        let Some(source) = order.source_of(position) else {
            return Err(SaveError::PageTree {
                path: destination.to_path_buf(),
                detail: format!("no source for position {position}"),
            });
        };
        let id = pages_by_document
            .get(source.document)
            .and_then(|pages| pages.get(&source))
            .copied()
            .ok_or_else(|| SaveError::PageTree {
                path: sources
                    .get(source.document)
                    .cloned()
                    .unwrap_or_else(|| destination.to_path_buf()),
                detail: format!("{source:?} is not in that document's page tree"),
            })?;
        kids.push(Object::Reference(id));
    }

    // Every retained page hangs directly off the root now, so its `/Parent` has to say
    // so. Wrong `/Parent` links are the kind of damage that opens fine and then
    // behaves oddly in another reader.
    let retained: Vec<ObjectId> = kids
        .iter()
        .filter_map(|kid| kid.as_reference().ok())
        .collect();
    for id in retained {
        if let Ok(page) = primary.get_dictionary_mut(id) {
            page.set("Parent", Object::Reference(root));
        }
    }

    let count = i64::try_from(kids.len()).map_err(|_| SaveError::PageTree {
        path: destination.to_path_buf(),
        detail: "impossibly many pages".to_owned(),
    })?;
    let tree = primary
        .get_dictionary_mut(root)
        .map_err(|error| SaveError::PageTree {
            path: destination.to_path_buf(),
            detail: error.to_string(),
        })?;
    tree.set("Kids", Object::Array(kids));
    tree.set("Count", Object::Integer(count));

    // Unreferenced now — pages left behind by a delete, and everything a merge
    // pulled in from a secondary document that no retained page depends on. Pruning
    // keeps the file from carrying content nobody can see any more, which matters
    // for more than size: a page left in the file is still extractable.
    primary.prune_objects();

    write_atomically(&mut primary, destination)
}

/// The object id of a document's root `Pages` node.
fn page_tree_root(document: &LoDocument, path: &Path) -> Result<ObjectId, SaveError> {
    let complain = |detail: String| SaveError::PageTree {
        path: path.to_path_buf(),
        detail,
    };
    document
        .catalog()
        .map_err(|error| complain(error.to_string()))?
        .get(b"Pages")
        .map_err(|error| complain(error.to_string()))?
        .as_reference()
        .map_err(|error| complain(error.to_string()))
}

/// Refuses a page tree whose root has branches rather than pages.
fn ensure_flat(document: &LoDocument, root: ObjectId, path: &Path) -> Result<(), SaveError> {
    let kids = document
        .get_dictionary(root)
        .and_then(|dict| dict.get(b"Kids"))
        .and_then(Object::as_array)
        .map_err(|error| SaveError::PageTree {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?;

    for kid in kids {
        let is_branch = kid
            .as_reference()
            .ok()
            .and_then(|id| document.get_dictionary(id).ok())
            .and_then(|dict| dict.get(b"Type").ok())
            .and_then(|kind| kind.as_name().ok())
            .is_some_and(|name| name == b"Pages");
        if is_branch {
            return Err(SaveError::NestedPageTree {
                path: path.to_path_buf(),
            });
        }
    }
    Ok(())
}

/// The stable [`Source`]s `document`'s file is expected to physically hold, per
/// [`PageOrder::on_disk`].
///
/// A plain index would be just as correct — `order` is checked against `sources` by
/// the caller's `assert_eq!` — but indexing panics on a mismatch the type system
/// cannot rule out, and a save is exactly the place that should refuse instead.
fn on_disk_pages<'order>(
    order: &'order PageOrder,
    document: usize,
    path: &Path,
) -> Result<&'order [Source], SaveError> {
    order.on_disk(document).ok_or_else(|| SaveError::PageTree {
        path: path.to_path_buf(),
        detail: format!("no on-disk record for document {document}"),
    })
}

/// Checks a loaded document's page count against what `on_disk` expects it to
/// physically hold, and returns each of those `Source`s' object id.
fn page_ids_by_source(
    document: &LoDocument,
    path: &Path,
    on_disk: &[Source],
) -> Result<HashMap<Source, ObjectId>, SaveError> {
    let pages = document.get_pages();
    if pages.len() != on_disk.len() {
        return Err(SaveError::PageCountMismatch {
            path: path.to_path_buf(),
            opened: on_disk.len(),
            found: pages.len(),
        });
    }
    // `get_pages` is keyed by one-based page number in document order, so the
    // physical index `n` is page `n + 1` — the only place that conversion happens.
    // Zipped against `on_disk` rather than returned by physical index alone, because
    // that index only means `source.page` for a document whose file has never been
    // the destination of a save — see the module docs.
    Ok(on_disk.iter().copied().zip(pages.into_values()).collect())
}

/// Writes beside the destination and renames into place.
fn write_atomically(document: &mut LoDocument, destination: &Path) -> Result<(), SaveError> {
    let complain = |detail: String| SaveError::Write {
        path: destination.to_path_buf(),
        detail,
    };

    // Beside the destination, not in a temp directory: a rename across volumes is a
    // copy, which is neither atomic nor guaranteed to be possible.
    let mut temporary = destination.as_os_str().to_owned();
    temporary.push(".porpoise-partial");
    let temporary = PathBuf::from(temporary);

    document
        .save(&temporary)
        .map_err(|error| complain(error.to_string()))?;

    if let Err(error) = std::fs::rename(&temporary, destination) {
        // Leaving the partial file behind would be worse than the failure itself.
        let _ = std::fs::remove_file(&temporary);
        return Err(complain(error.to_string()));
    }
    Ok(())
}

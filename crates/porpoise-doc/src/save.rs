//! Writing a reordered document back out.
//!
//! The only part of this crate that uses `lopdf`, and the only part that writes to
//! disk. Both facts drive the shape: guards first, then a write that cannot leave a
//! half-finished file where a working one used to be.
//!
//! # Why two parsers, and what keeps them honest
//!
//! Pages are opened and rasterized with `hayro`, which cannot write PDFs, so saving
//! uses `lopdf`. The file is therefore parsed twice, and the two have to agree on
//! what "page 3" is — if they disagree, a reorder moves the wrong page and writes the
//! mistake to disk. [`save_reordered`] refuses rather than guessing.
//!
//! Measured on three real drawing sets before relying on it: hayro and lopdf agreed on
//! the page count for all three, and all three had flat page trees. See
//! `docs/goal-4-plan.md` §2.

use std::path::{Path, PathBuf};

use lopdf::{Document as LoDocument, Object, ObjectId};

use crate::PageOrder;

/// Why a document could not be saved.
///
/// Every variant means "nothing was written". A refusal has to be distinguishable
/// from a partial write, because the whole point is that the original survives.
#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    /// The source could not be read or parsed for writing.
    #[error("could not read {path} for saving: {detail}")]
    Source {
        /// The file we tried to read.
        path: PathBuf,
        /// The parser's complaint.
        detail: String,
    },
    /// The two parsers disagree about how many pages the document has.
    ///
    /// Refused rather than guessed. If `lopdf` sees a different set of pages than
    /// `hayro` did, then the positions being reordered do not mean what the person
    /// reordering them saw, and the result would be scrambled in a way nobody notices
    /// until later.
    #[error(
        "cannot edit {path} safely: it was opened with {opened} page(s) but reads as \
         {found} for writing"
    )]
    PageCountMismatch {
        /// The file.
        path: PathBuf,
        /// What the viewer opened.
        opened: usize,
        /// What the writer found.
        found: usize,
    },
    /// The page tree has branches, so reordering could change what pages inherit.
    ///
    /// In a nested tree a page can inherit `/Resources`, `/MediaBox`, `/Rotate` or
    /// `/CropBox` from the branch above it. Moving pages between branches changes
    /// that, and `lopdf` 0.44 offers no inherited-attribute support, so doing it
    /// correctly means pushing those attributes down onto each page first. Not yet
    /// implemented; refused meanwhile, because the failure mode is a document that
    /// renders wrong rather than one that fails to open.
    #[error("cannot edit {path} yet: its page tree is nested, which this cannot reorder safely")]
    NestedPageTree {
        /// The file.
        path: PathBuf,
    },
    /// The page tree could not be understood at all.
    #[error("could not read the page tree of {path}: {detail}")]
    PageTree {
        /// The file.
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

/// Writes `source` to `destination` with its pages in the order `order` describes.
///
/// The write is atomic: the new document goes to a temporary file beside the
/// destination and is renamed into place. A rename within one directory either happens
/// or does not, so an interrupted save leaves the original intact and a stray temp
/// file, never a truncated PDF.
///
/// `opened_page_count` is what the viewer saw when it opened the document, and is
/// checked against what the writer finds. See [`SaveError::PageCountMismatch`].
pub fn save_reordered(
    source: &Path,
    order: &PageOrder,
    destination: &Path,
    overwrite: Overwrite,
) -> Result<(), SaveError> {
    if overwrite == Overwrite::Refuse && destination.exists() {
        return Err(SaveError::WouldOverwrite {
            path: destination.to_path_buf(),
        });
    }

    let mut document = LoDocument::load(source).map_err(|error| SaveError::Source {
        path: source.to_path_buf(),
        detail: error.to_string(),
    })?;

    let pages = document.get_pages();
    if pages.len() != order.source_len() {
        return Err(SaveError::PageCountMismatch {
            path: source.to_path_buf(),
            opened: order.source_len(),
            found: pages.len(),
        });
    }

    let root = page_tree_root(&document, source)?;
    ensure_flat(&document, root, source)?;

    // `get_pages` is keyed by one-based page number in document order, so the source
    // index `n` is page `n + 1`. The only place that conversion happens.
    let ids: Vec<ObjectId> = pages.into_values().collect();
    let mut kids = Vec::with_capacity(order.len());
    for position in 0..order.len() {
        let source_index = order
            .source_of(position)
            .ok_or_else(|| SaveError::PageTree {
                path: source.to_path_buf(),
                detail: format!("no source page for position {position}"),
            })?;
        let id = *ids.get(source_index).ok_or_else(|| SaveError::PageTree {
            path: source.to_path_buf(),
            detail: format!("source page {source_index} is not in the page tree"),
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
        if let Ok(page) = document.get_dictionary_mut(id) {
            page.set("Parent", Object::Reference(root));
        }
    }

    let count = i64::try_from(kids.len()).map_err(|_| SaveError::PageTree {
        path: source.to_path_buf(),
        detail: "impossibly many pages".to_owned(),
    })?;
    let tree = document
        .get_dictionary_mut(root)
        .map_err(|error| SaveError::PageTree {
            path: source.to_path_buf(),
            detail: error.to_string(),
        })?;
    tree.set("Kids", Object::Array(kids));
    tree.set("Count", Object::Integer(count));

    // Deleted pages are now unreferenced. Pruning keeps the file from carrying content
    // the document no longer shows — which matters for more than size: a deleted page
    // left in the file is still extractable.
    document.prune_objects();

    write_atomically(&mut document, destination)
}

/// The object id of the document's root `Pages` node.
fn page_tree_root(document: &LoDocument, source: &Path) -> Result<ObjectId, SaveError> {
    let complain = |detail: String| SaveError::PageTree {
        path: source.to_path_buf(),
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
fn ensure_flat(document: &LoDocument, root: ObjectId, source: &Path) -> Result<(), SaveError> {
    let kids = document
        .get_dictionary(root)
        .and_then(|dict| dict.get(b"Kids"))
        .and_then(Object::as_array)
        .map_err(|error| SaveError::PageTree {
            path: source.to_path_buf(),
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
                path: source.to_path_buf(),
            });
        }
    }
    Ok(())
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

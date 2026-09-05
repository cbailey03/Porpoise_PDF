//! Pushing inherited page attributes down onto the pages that inherit them.
//!
//! A PDF page tree may nest, and four attributes flow down it: `/Resources`,
//! `/MediaBox`, `/CropBox` and `/Rotate`. A page that does not define one of them
//! uses the nearest definition above it.
//!
//! Saving a reordered document flattens that tree so every page hangs off the root,
//! which throws the structure away. If the values are not written onto the pages
//! first, a page that inherited from a branch silently changes size, changes
//! rotation, or loses the fonts it drew with. The document still opens, so nothing
//! reports an error; it just renders wrong.
//!
//! [`push_down`] runs before the flatten in [`crate::save`] and makes every page
//! self-describing, which is what lets a nested document be edited at all.

use std::collections::HashSet;

use lopdf::{Document as LoDocument, Object, ObjectId};

/// The attributes a page inherits from the nodes above it.
///
/// Fixed by the PDF specification: these four and no others. They are inherited,
/// never merged. A branch's `/Resources` replaces the one above it wholesale rather
/// than adding to it, so only the nearest definition matters. Merging them would be
/// the subtle way to get this wrong.
const INHERITABLE: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];

/// How deep the page tree may nest before we refuse to walk it.
///
/// Matches `lopdf`'s own `PAGE_TREE_DEPTH_LIMIT`. The save path already trusts
/// `Document::get_pages` to enumerate the pages, so a shallower limit here would
/// refuse a document that the page count check just accepted, for a reason nobody
/// reading the file could see.
const MAX_DEPTH: usize = 256;

/// The attributes in force at some point in the tree, nearest definition last.
type Attributes = Vec<(Vec<u8>, Object)>;

/// Why the inherited attributes could not be resolved.
///
/// Every variant means nothing was changed. [`crate::save`] folds these into
/// `SaveError::PageTree`, so the caller sees one "could not read the page tree"
/// failure whatever the shape of the damage.
#[derive(Debug, thiserror::Error)]
pub(crate) enum InheritError {
    /// A branch node is reachable from itself, so the tree is not a tree.
    ///
    /// Untrusted input, so this is a real case rather than a theoretical one: a
    /// `/Kids` cycle would otherwise recurse until the stack ran out.
    #[error("the page tree reaches node {0:?} twice, so it is not a tree")]
    Repeated(ObjectId),
    /// The tree nests deeper than [`MAX_DEPTH`].
    #[error("the page tree nests more than {MAX_DEPTH} levels deep")]
    TooDeep,
    /// A node could not be read.
    #[error("{0}")]
    Malformed(String),
}

/// Writes every inherited attribute onto the page that inherits it.
///
/// Afterwards each page defines all four attributes it actually uses, so moving it
/// anywhere in the document cannot change how it renders.
///
/// Nothing is removed from the nodes above. A page's own value already shadows an
/// ancestor's, so a leftover attribute on the root is harmless, while removing one
/// that some page still relies on changes rendering with nothing to show for it.
pub(crate) fn push_down(document: &mut LoDocument, root: ObjectId) -> Result<(), InheritError> {
    let mut pending: Vec<(ObjectId, Attributes)> = Vec::new();
    collect(
        document,
        root,
        &Attributes::new(),
        0,
        &mut HashSet::new(),
        &mut pending,
    )?;

    for (page, attributes) in pending {
        let Ok(dictionary) = document.get_dictionary_mut(page) else {
            continue;
        };
        for (key, value) in attributes {
            // The page's own value wins over anything above it, so only fill gaps.
            if !dictionary.has(&key) {
                dictionary.set(key, value);
            }
        }
    }
    Ok(())
}

/// Walks one branch, recording what each page below it should inherit.
///
/// Only ever called on a branch node, which is why `visited` guards branches alone.
/// A leaf reached twice is malformed but harmless: applying the same attributes to
/// it twice is idempotent, and refusing would reject flat documents that saved fine
/// before. A branch reached twice is a cycle, and has to stop.
fn collect(
    document: &LoDocument,
    node: ObjectId,
    inherited: &Attributes,
    depth: usize,
    visited: &mut HashSet<ObjectId>,
    out: &mut Vec<(ObjectId, Attributes)>,
) -> Result<(), InheritError> {
    if depth > MAX_DEPTH {
        return Err(InheritError::TooDeep);
    }
    if !visited.insert(node) {
        return Err(InheritError::Repeated(node));
    }

    let dictionary = document
        .get_dictionary(node)
        .map_err(|error| InheritError::Malformed(error.to_string()))?;

    // What this node defines replaces what it was given, for everything below it.
    let mut in_force = inherited.clone();
    for key in INHERITABLE {
        if let Ok(value) = dictionary.get(key) {
            in_force.retain(|(defined, _)| defined != key);
            in_force.push((key.to_vec(), value.clone()));
        }
    }

    // `/Kids` is dereferenced rather than read directly, because it may itself be an
    // indirect object. `lopdf`'s own page walk does the same, and the two have to
    // agree on what the tree contains.
    let kids = dictionary
        .get_deref(b"Kids", document)
        .and_then(Object::as_array)
        .map_err(|error| InheritError::Malformed(error.to_string()))?;

    for kid in kids {
        let Ok(id) = kid.as_reference() else { continue };
        let Ok(kid_dictionary) = document.get_dictionary(id) else {
            continue;
        };
        // An untyped kid is skipped, matching `lopdf`, which counts neither a page
        // nor a branch without a `/Type`. Skipping keeps this walk and the page
        // enumeration looking at the same set of pages.
        if kid_dictionary.has_type(b"Page") {
            out.push((id, in_force.clone()));
        } else if kid_dictionary.has_type(b"Pages") {
            collect(document, id, &in_force, depth + 1, visited, out)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use lopdf::{Dictionary, dictionary};

    use super::*;

    /// Adds a leaf page carrying whatever `own` defines.
    fn page(document: &mut LoDocument, own: Dictionary) -> ObjectId {
        let mut leaf = dictionary! { "Type" => "Page" };
        for (key, value) in own.iter() {
            leaf.set(key.clone(), value.clone());
        }
        document.add_object(leaf)
    }

    /// Adds a `/Pages` node over `kids`, carrying whatever `own` defines.
    fn branch(document: &mut LoDocument, kids: &[ObjectId], own: Dictionary) -> ObjectId {
        let mut node = dictionary! {
            "Type" => "Pages",
            "Kids" => Object::Array(kids.iter().copied().map(Object::Reference).collect()),
            "Count" => Object::Integer(kids.len() as i64),
        };
        for (key, value) in own.iter() {
            node.set(key.clone(), value.clone());
        }
        document.add_object(node)
    }

    /// The `/Rotate` a page ends up with, if any.
    fn rotate(document: &LoDocument, page: ObjectId) -> Option<i64> {
        document
            .get_dictionary(page)
            .ok()?
            .get(b"Rotate")
            .ok()?
            .as_i64()
            .ok()
    }

    /// Whether a page defines a key at all.
    fn defines(document: &LoDocument, page: ObjectId, key: &[u8]) -> bool {
        document
            .get_dictionary(page)
            .is_ok_and(|dictionary| dictionary.has(key))
    }

    #[test]
    fn a_page_takes_the_root_value_it_was_inheriting() {
        let mut document = LoDocument::new();
        let leaf = page(&mut document, Dictionary::new());
        let root = branch(&mut document, &[leaf], dictionary! { "Rotate" => 90 });

        push_down(&mut document, root).unwrap();

        // The root keeps its own copy; the point is that the page no longer needs it.
        assert_eq!(rotate(&document, leaf), Some(90));
    }

    #[test]
    fn a_page_takes_the_nearest_definition_above_it() {
        let mut document = LoDocument::new();
        let leaf = page(&mut document, Dictionary::new());
        let middle = branch(&mut document, &[leaf], dictionary! { "Rotate" => 180 });
        let root = branch(&mut document, &[middle], dictionary! { "Rotate" => 90 });

        push_down(&mut document, root).unwrap();

        // Nearest wins. Taking the root's 90 here is exactly the bug this prevents.
        assert_eq!(rotate(&document, leaf), Some(180));
    }

    #[test]
    fn a_page_keeps_the_value_it_defines_itself() {
        let mut document = LoDocument::new();
        let leaf = page(&mut document, dictionary! { "Rotate" => 270 });
        let middle = branch(&mut document, &[leaf], dictionary! { "Rotate" => 180 });
        let root = branch(&mut document, &[middle], dictionary! { "Rotate" => 90 });

        push_down(&mut document, root).unwrap();

        assert_eq!(rotate(&document, leaf), Some(270));
    }

    #[test]
    fn siblings_under_different_branches_keep_different_values() {
        let mut document = LoDocument::new();
        let left_page = page(&mut document, Dictionary::new());
        let right_page = page(&mut document, Dictionary::new());
        let left = branch(&mut document, &[left_page], dictionary! { "Rotate" => 90 });
        let right = branch(
            &mut document,
            &[right_page],
            dictionary! { "Rotate" => 180 },
        );
        let root = branch(&mut document, &[left, right], Dictionary::new());

        push_down(&mut document, root).unwrap();

        // The whole reason a flatten is dangerous: these two are about to become
        // siblings under one root, and they must not converge on one value.
        assert_eq!(rotate(&document, left_page), Some(90));
        assert_eq!(rotate(&document, right_page), Some(180));
    }

    #[test]
    fn three_levels_resolve_to_the_innermost_definition() {
        let mut document = LoDocument::new();
        let leaf = page(&mut document, Dictionary::new());
        let inner = branch(&mut document, &[leaf], dictionary! { "Rotate" => 270 });
        let middle = branch(&mut document, &[inner], dictionary! { "Rotate" => 180 });
        let root = branch(&mut document, &[middle], dictionary! { "Rotate" => 90 });

        push_down(&mut document, root).unwrap();

        assert_eq!(rotate(&document, leaf), Some(270));
    }

    #[test]
    fn all_four_inheritable_attributes_are_pushed_down() {
        let mut document = LoDocument::new();
        let leaf = page(&mut document, Dictionary::new());
        let root = branch(
            &mut document,
            &[leaf],
            dictionary! {
                "Resources" => Dictionary::new(),
                "MediaBox" => Object::Array(vec![
                    Object::Integer(0),
                    Object::Integer(0),
                    Object::Integer(400),
                    Object::Integer(400),
                ]),
                "CropBox" => Object::Array(vec![
                    Object::Integer(0),
                    Object::Integer(0),
                    Object::Integer(200),
                    Object::Integer(200),
                ]),
                "Rotate" => 90,
            },
        );

        push_down(&mut document, root).unwrap();

        for key in INHERITABLE {
            assert!(
                defines(&document, leaf, key),
                "{} was not pushed down",
                String::from_utf8_lossy(key)
            );
        }
    }

    #[test]
    fn nothing_is_invented_for_attributes_nobody_defined() {
        let mut document = LoDocument::new();
        let leaf = page(&mut document, Dictionary::new());
        let root = branch(&mut document, &[leaf], Dictionary::new());

        push_down(&mut document, root).unwrap();

        // A flat document with no inherited attributes has to come out untouched.
        // Writing a default `/MediaBox` here would change how the page renders.
        for key in INHERITABLE {
            assert!(
                !defines(&document, leaf, key),
                "{} was invented",
                String::from_utf8_lossy(key)
            );
        }
    }

    #[test]
    fn a_cycle_in_the_page_tree_is_refused() {
        let mut document = LoDocument::new();
        let leaf = page(&mut document, Dictionary::new());
        let inner = branch(&mut document, &[leaf], Dictionary::new());
        let root = branch(&mut document, &[inner], Dictionary::new());
        // Point the inner branch back at the root. Without the visited set this
        // recurses until the stack is gone, which is a crash on untrusted input.
        document
            .get_dictionary_mut(inner)
            .unwrap()
            .set("Kids", Object::Array(vec![Object::Reference(root)]));

        let error = push_down(&mut document, root).unwrap_err();

        assert!(matches!(error, InheritError::Repeated(_)), "got {error:?}");
    }

    #[test]
    fn a_tree_deeper_than_the_limit_is_refused() {
        let mut document = LoDocument::new();
        let leaf = page(&mut document, Dictionary::new());
        let mut current = branch(&mut document, &[leaf], Dictionary::new());
        for _ in 0..=MAX_DEPTH {
            current = branch(&mut document, &[current], Dictionary::new());
        }

        let error = push_down(&mut document, current).unwrap_err();

        assert!(matches!(error, InheritError::TooDeep), "got {error:?}");
    }

    #[test]
    fn a_tree_at_the_limit_is_still_walked() {
        let mut document = LoDocument::new();
        let leaf = page(&mut document, dictionary! {});
        let mut current = branch(&mut document, &[leaf], dictionary! { "Rotate" => 90 });
        for _ in 0..MAX_DEPTH - 1 {
            current = branch(&mut document, &[current], Dictionary::new());
        }

        push_down(&mut document, current).unwrap();

        assert_eq!(rotate(&document, leaf), Some(90));
    }

    #[test]
    fn a_page_listed_twice_is_allowed() {
        let mut document = LoDocument::new();
        let leaf = page(&mut document, Dictionary::new());
        let root = branch(&mut document, &[leaf, leaf], dictionary! { "Rotate" => 90 });

        // Malformed, but flat documents like this saved fine before this module
        // existed, and refusing them now would be a regression. Applying the same
        // attributes twice is idempotent.
        push_down(&mut document, root).unwrap();

        assert_eq!(rotate(&document, leaf), Some(90));
    }

    #[test]
    fn a_branch_without_kids_is_refused() {
        let mut document = LoDocument::new();
        let root = document.add_object(dictionary! { "Type" => "Pages" });

        let error = push_down(&mut document, root).unwrap_err();

        assert!(matches!(error, InheritError::Malformed(_)), "got {error:?}");
    }

    #[test]
    fn an_untyped_kid_is_skipped_rather_than_treated_as_a_page() {
        let mut document = LoDocument::new();
        let stray = document.add_object(dictionary! { "Contents" => Object::Null });
        let leaf = page(&mut document, Dictionary::new());
        let root = branch(
            &mut document,
            &[stray, leaf],
            dictionary! { "Rotate" => 90 },
        );

        push_down(&mut document, root).unwrap();

        // `lopdf` does not count an untyped kid as a page, so neither does this. If
        // the two disagreed about what the tree holds, the reorder would be off by
        // one and write the mistake to disk.
        assert!(!defines(&document, stray, b"Rotate"));
        assert_eq!(rotate(&document, leaf), Some(90));
    }
}

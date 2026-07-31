//! One shape for every button in the window.
//!
//! There used to be one helper — `edit_button` in [`crate::chrome`] — covering the five
//! page-edit buttons, and every other control called egui directly. That worked while the
//! buttons all lived in one function next to each other, and it had already drifted by the
//! time there were twenty of them in two files: the question box's **Cancel** was the only
//! one with no tooltip, and there were three different ways of deciding whether a control
//! was greyed out.
//!
//! # The command *is* the enabled state
//!
//! This is `edit_button`'s idea, and the reason it is worth generalizing rather than
//! deleting. A control is handed an `Option` of whatever it produces, and that same
//! `Option` decides whether it is greyed out. So a lit button cannot mean something a key
//! press does not — there is no way to draw an enabled button and *then* work out what it
//! does, because the payload had to exist first. See [`crate::edits`], which is where the
//! toolbar's `Option`s come from.
//!
//! # Nothing here dispatches
//!
//! Every function returns what the click produced and does nothing with it, the same way
//! [`crate::chrome`] hands its commands back rather than running them. That is what lets
//! these be generic: the toolbar collects [`crate::command::Command`]s, the question box
//! collects a [`crate::confirm::Answer`], the grid's tabs collect a mode and its search box
//! collects a string. One shape, because none of them needs to know what the payload means.
//!
//! # Every symbol on a button is named here
//!
//! Not decoration: egui cannot draw most symbol characters, and the ones it cannot draw
//! come out as an **empty box** rather than as any kind of error. So the symbols are a
//! closed list — [`Glyph`] — and [`tests::every_glyph_can_be_drawn`] checks the whole list
//! against the fonts the window will actually use.

use eframe::egui;

/// A symbol a button is labelled with, instead of a word.
///
/// A closed list on purpose, because a symbol that egui has no glyph for draws an empty box
/// and nothing complains. That has happened twice: the toolbar wanted U+2191/U+2193 for
/// **Up** and **Down** and got boxes, and the page grid's clear button shipped as U+2715 and
/// drew a box for two commits. Both were found by a person looking at a screenshot, which is
/// not a mechanism.
///
/// So: words wherever they fit — that is why **Up** and **Down** are words — and for the
/// buttons too small for a word, a symbol from this list, which is tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Glyph {
    /// Jump to the first page.
    FirstPage,
    /// Jump to the last page.
    LastPage,
    /// One zoom rung down.
    ZoomOut,
    /// One zoom rung up.
    ZoomIn,
    /// Empty the page grid's search box.
    ClearSearch,
}

impl Glyph {
    /// Every glyph, for the test that checks they can all be drawn.
    ///
    /// Kept exhaustive by [`tests::every_glyph_is_listed`], which matches on each variant
    /// and so fails to compile when one is added — the same mechanism
    /// [`crate::thumbnails::GridMode::EVERY`] uses. An unlisted glyph would be an untested
    /// one, which is the whole thing this is here to prevent.
    ///
    /// Test-only, unlike `GridMode::EVERY`: nothing the window does needs to walk the
    /// glyphs, only to check them. The enforcement therefore lands when the tests are
    /// compiled rather than when the binary is, which is where it is needed.
    #[cfg(test)]
    pub(crate) const EVERY: [Self; 5] = [
        Self::FirstPage,
        Self::LastPage,
        Self::ZoomOut,
        Self::ZoomIn,
        Self::ClearSearch,
    ];

    /// What it draws as.
    pub(crate) fn text(self) -> &'static str {
        match self {
            // U+23EE and U+23ED, from egui's bundled emoji-icon-font.
            Self::FirstPage => "⏮",
            Self::LastPage => "⏭",
            // U+2212 minus, not an ASCII hyphen: it is the width of the `+` beside it, so
            // the two zoom buttons come out the same size.
            Self::ZoomOut => "−",
            Self::ZoomIn => "+",
            // U+2716, not the U+2715 this shipped with. See the type's docs.
            Self::ClearSearch => "✖",
        }
    }
}

/// A button that does one thing, and whether it can do it right now.
pub(crate) struct Action<'a, T> {
    /// What the button says.
    pub(crate) text: &'a str,
    /// The tooltip.
    ///
    /// Not an `Option`, deliberately. The one button in the window that had no tooltip was
    /// the question box's **Cancel**, and it was missing for no better reason than that
    /// nothing asked for one. A required field is the cheapest fix that cannot recur.
    pub(crate) hover: &'a str,
    /// What a click produces, or `None` to grey the button out.
    pub(crate) produces: Option<T>,
}

/// A button that stays down, for something that is either on or off.
pub(crate) struct Toggle<'a, T> {
    /// What the toggle says.
    pub(crate) text: &'a str,
    /// The tooltip. Required, for the reason [`Action::hover`] is.
    pub(crate) hover: &'a str,
    /// Whether it is on, which is what gives it the pressed look.
    pub(crate) on: bool,
    /// What a click produces.
    ///
    /// Not an `Option`, unlike [`Action::produces`]: every toggle in the window can always
    /// be switched, and one that could not would be a label. If a greyed-out toggle is ever
    /// needed, that is the point to widen this — not before.
    pub(crate) produces: T,
}

/// Draws a button. Returns what it produces, if it was clicked.
pub(crate) fn button<T>(ui: &mut egui::Ui, action: Action<'_, T>) -> Option<T> {
    let Action {
        text,
        hover,
        produces,
    } = action;
    draw(ui, egui::Button::new(text), hover, produces)
}

/// The same button, shrunk to sit beside a text field.
///
/// A separate function rather than a size field on [`Action`], so the twenty call sites
/// that want the ordinary size do not each have to say so.
pub(crate) fn small_button<T>(ui: &mut egui::Ui, action: Action<'_, T>) -> Option<T> {
    let Action {
        text,
        hover,
        produces,
    } = action;
    draw(ui, egui::Button::new(text).small(), hover, produces)
}

/// Draws a toggle. Returns what it produces, if it was clicked.
///
/// Clicking one that is already on still produces its payload. Whether that means anything
/// is the caller's to decide: the toolbar's zoom toggles are idempotent, while the grid's
/// tabs drop it.
pub(crate) fn toggle<T>(ui: &mut egui::Ui, state: Toggle<'_, T>) -> Option<T> {
    let Toggle {
        text,
        hover,
        on,
        produces,
    } = state;
    ui.selectable_label(on, text)
        .on_hover_text(hover)
        .clicked()
        .then_some(produces)
}

/// Shared by both button sizes: enabled exactly when there is something to produce.
fn draw<T>(
    ui: &mut egui::Ui,
    widget: egui::Button<'_>,
    hover: &str,
    produces: Option<T>,
) -> Option<T> {
    if ui
        .add_enabled(produces.is_some(), widget)
        .on_hover_text(hover)
        .clicked()
    {
        produces
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use eframe::egui::epaint::text::{FontDefinitions, Fonts, TextOptions};

    use super::*;

    /// Every character the window can draw, and which bundled font supplies it.
    ///
    /// Built from egui's default fonts, because that is what the program runs with —
    /// nothing in this crate calls `set_fonts`. Needs no window, no GPU and no document:
    /// text layout stands alone, which is the only reason any of this is a unit test when
    /// nothing else about the toolbar is — see [`crate::chrome`].
    ///
    /// # Not `Fonts::has_glyphs`
    ///
    /// That is the obvious API and it is wrong for this. It answers "is this character
    /// owned by some face other than the *replacement* face", and the replacement square
    /// `◻` lives in NotoEmoji — so every character NotoEmoji owns is reported missing while
    /// drawing perfectly. `✖` is one of them. Pinned by
    /// [`has_glyphs_is_not_the_api_for_this`], so nobody simplifies this back.
    ///
    /// This map instead comes from each face's character map, which is what the renderer
    /// itself resolves a character against.
    fn drawable() -> BTreeMap<char, Vec<String>> {
        // Proportional, which is what widget text uses, and deliberately not monospace.
        // That distinction *is* the bug: `Hack` has plenty of symbols and is only in the
        // monospace family, so a character can be in the bundled fonts and still undrawable
        // in a button. The toolbar's rejected U+2191 is exactly that case.
        Fonts::new(TextOptions::default(), FontDefinitions::default())
            .fonts
            .font(&egui::FontFamily::Proportional)
            .characters()
            .clone()
    }

    #[test]
    fn every_glyph_is_listed() {
        // The enforcement: a variant added without being put in `EVERY` fails to compile
        // here, and an unlisted glyph is one nothing below checks.
        for glyph in Glyph::EVERY {
            match glyph {
                Glyph::FirstPage
                | Glyph::LastPage
                | Glyph::ZoomOut
                | Glyph::ZoomIn
                | Glyph::ClearSearch => {}
            }
        }
        assert_eq!(Glyph::EVERY.len(), 5);
    }

    #[test]
    fn every_glyph_can_be_drawn() {
        // The point of the module. A character no face has is drawn as an empty box,
        // silently, and until now only a person looking at a screenshot would notice.
        let drawable = drawable();
        for glyph in Glyph::EVERY {
            for character in glyph.text().chars() {
                assert!(
                    drawable.contains_key(&character),
                    "{glyph:?} is {:?}, and U+{:04X} is in none of egui's proportional \
                     fonts — it will draw as an empty box. Pick another character, or a word.",
                    glyph.text(),
                    character as u32
                );
            }
        }
    }

    #[test]
    fn a_character_no_font_has_is_reported_as_missing() {
        // Proof the check above can fail. Without it, a `drawable` that wrongly returned
        // every character would look like a passing suite rather than a broken test.
        //
        // U+2715 is the character the clear button shipped with, and the box it drew is what
        // started all of this. If this ever fails, egui has gained the glyph — good news,
        // and the answer is to delete this test rather than work around it.
        assert!(
            !drawable().contains_key(&'\u{2715}'),
            "egui can now draw U+2715, so this test has outlived its purpose"
        );
    }

    #[test]
    fn has_glyphs_is_not_the_api_for_this() {
        // Why `drawable` walks character maps instead of asking the obvious question.
        // `✖` draws correctly — confirmed in a real window, and asserted above — yet
        // `has_glyphs` says no, because NotoEmoji owns both `✖` and the replacement
        // square that "missing" is defined as.
        //
        // A failure here means epaint has fixed that, and the simpler API becomes usable.
        let mut fonts = Fonts::new(TextOptions::default(), FontDefinitions::default());
        assert!(
            !fonts.has_glyphs(&egui::FontId::proportional(14.0), Glyph::ClearSearch.text()),
            "has_glyphs is sound now — `drawable` can go back to using it"
        );
    }

    #[test]
    fn the_non_ascii_in_a_word_label_can_be_drawn() {
        // [`Glyph`] covers the buttons labelled with a symbol. **Open…** is a word label
        // that happens to hold one non-ASCII character, so it is checked here rather than
        // bent into the enum. Any future word label with one belongs on this list.
        let drawable = drawable();
        for label in ["Open…", "Add pages…", "Stage a file…"] {
            for character in label.chars() {
                assert!(
                    drawable.contains_key(&character),
                    "{label:?} holds U+{:04X}, which egui draws as an empty box",
                    character as u32
                );
            }
        }
    }
}

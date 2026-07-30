//! Showing a path to a person in limited space.
//!
//! Its own module because this was written three times independently — the window title,
//! the save destination in the status bar, and the drop hint — each with the same
//! fallback and none of them tested. A window is narrower than a path, so every one of
//! them wanted the file name.

use std::path::Path;

/// The file name, or the whole path when there is no file name to take.
///
/// The fallback matters more than it looks. A path can perfectly well end in `..`, `/`,
/// or a bare drive letter, and none of those have a file name — printing an empty label
/// for one would read as a bug in whatever produced the path.
pub(crate) fn file_label(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;

    #[test]
    fn a_path_shows_as_its_file_name() {
        assert_eq!(file_label(Path::new("plans/2026/sheet.pdf")), "sheet.pdf");
        assert_eq!(file_label(Path::new("sheet.pdf")), "sheet.pdf");
    }

    #[test]
    fn a_windows_path_shows_as_its_file_name() {
        assert_eq!(
            file_label(Path::new(r"C:\Users\me\Desktop\ROLT14_GDOT-U_6.pdf")),
            "ROLT14_GDOT-U_6.pdf"
        );
    }

    #[test]
    fn a_path_with_no_file_name_falls_back_to_the_whole_thing() {
        // Never empty: a blank label in the status bar reads as something broken rather
        // than as a path that happens to have no last component.
        for odd in ["..", "/", ""] {
            let shown = file_label(Path::new(odd));
            assert_eq!(shown, odd, "{odd:?} did not fall back to itself");
        }
    }

    #[test]
    fn a_directory_shows_as_its_last_component() {
        assert_eq!(file_label(Path::new("plans/gdot/")), "gdot");
    }

    #[test]
    fn a_non_ascii_name_survives() {
        let path = PathBuf::from("café/naïve plan.pdf");
        assert_eq!(file_label(&path), "naïve plan.pdf");
    }
}

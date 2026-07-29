//! Turning a rasterized page into something egui can draw.
//!
//! Its own module because it is the boundary between the renderer's pixels and the
//! GPU, and a boundary is easier to trust when it is small enough to read in one
//! sitting. Nothing here touches viewer state.

use eframe::egui;
use porpoise_render::RenderedPage;

/// The whole texture, for `Painter::image`.
pub(crate) const FULL_UV: egui::Rect = egui::Rect {
    min: egui::Pos2 { x: 0.0, y: 0.0 },
    max: egui::Pos2 { x: 1.0, y: 1.0 },
};

/// Converts a rasterized page into an egui image, or `None` if it could not be
/// turned into a texture safely.
///
/// This is the last thing between the renderer and the GPU, and it exists because
/// both steps past it are fallible in ways that end the process rather than the
/// page:
///
/// - `ColorImage::from_rgba_unmultiplied` *panics* on a length mismatch, and a
///   panic on the UI thread takes down the window.
/// - `load_texture` hands the result to wgpu, which validates dimensions. A
///   zero-width or zero-height image passes the length check trivially — zero
///   bytes is exactly what `0 * h * 4` asks for — and then fails validation.
///
/// `HayroRenderer` refuses a sub-pixel page before either of these is reached, so
/// neither case is reachable through the shipped renderer today. The guard does
/// not rely on that: it is the boundary's job to hold whatever the [`Renderer`]
/// on the other side happens to return.
///
/// [`Renderer`]: porpoise_render::Renderer
pub(crate) fn to_color_image(page: &RenderedPage) -> Option<egui::ColorImage> {
    if page.width == 0 || page.height == 0 {
        return None;
    }
    let width = usize::try_from(page.width).ok()?;
    let height = usize::try_from(page.height).ok()?;
    let expected = width.checked_mul(height)?.checked_mul(4)?;
    if expected != page.rgba.len() {
        return None;
    }
    // Our buffers are non-premultiplied, which is what this constructor wants.
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [width, height],
        &page.rgba,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(width: u32, height: u32, bytes: usize) -> RenderedPage {
        RenderedPage {
            width,
            height,
            rgba: vec![0; bytes],
        }
    }

    #[test]
    fn a_consistent_buffer_converts() {
        let image = to_color_image(&page(4, 3, 4 * 3 * 4)).expect("4x3 RGBA should convert");
        assert_eq!(image.size, [4, 3]);
    }

    #[test]
    fn a_short_buffer_is_refused_rather_than_panicking() {
        // One byte short. `ColorImage::from_rgba_unmultiplied` would panic here,
        // on the UI thread, closing the window.
        assert!(to_color_image(&page(4, 3, 4 * 3 * 4 - 1)).is_none());
    }

    #[test]
    fn a_long_buffer_is_refused_too() {
        // Trailing bytes mean the renderer and the header disagree; we cannot tell
        // which is right, so refuse rather than display a guess.
        assert!(to_color_image(&page(4, 3, 4 * 3 * 4 + 1)).is_none());
    }

    #[test]
    fn a_zero_sized_page_is_refused() {
        assert!(to_color_image(&page(0, 3, 0)).is_none());
        assert!(to_color_image(&page(4, 0, 0)).is_none());
    }

    #[test]
    fn dimensions_that_would_overflow_are_refused() {
        assert!(to_color_image(&page(u32::MAX, u32::MAX, 16)).is_none());
    }
}

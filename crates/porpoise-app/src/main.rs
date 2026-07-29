//! `porpoise` — command line entry point.
//!
//! At M0 this reports what the viewer would lay out for a document, which is
//! enough to exercise the whole non-rendering path. The window arrives at M2; see
//! `docs/goal-1-plan.md`, section 4.

use std::error::Error;
use std::process::ExitCode;

use porpoise_doc::Document;
use porpoise_view::ScrollLayout;

/// Gap between pages in the scrolling column, in PDF points.
const PAGE_GAP_PT: f64 = 12.0;

/// Height of the notional viewport used to illustrate the visible set.
const SAMPLE_VIEWPORT_PT: f64 = 800.0;

fn main() -> ExitCode {
    let Some(path) = std::env::args_os().nth(1) else {
        eprintln!("usage: porpoise <file.pdf>");
        return ExitCode::FAILURE;
    };

    let document = match Document::open(&path) {
        Ok(document) => document,
        Err(error) => {
            eprintln!("error: {error}");
            let mut cause = error.source();
            while let Some(current) = cause {
                eprintln!("  caused by: {current}");
                cause = current.source();
            }
            return ExitCode::FAILURE;
        }
    };

    let layout = ScrollLayout::vertical(document.geometry(), PAGE_GAP_PT);

    println!("{}", path.to_string_lossy());
    println!("  pages:         {}", document.page_count());
    println!("  scroll height: {:.1} pt", layout.content_height_pt());
    println!("  widest page:   {:.1} pt", layout.content_width_pt());

    // Whether page sizes vary is the single most load-bearing fact about scroll
    // layout, so surface it rather than making someone diff the numbers.
    let mut sizes: Vec<(i64, i64)> = document
        .geometry()
        .iter()
        .map(|page| (page.width_pt.round() as i64, page.height_pt.round() as i64))
        .collect();
    sizes.sort_unstable();
    sizes.dedup();

    if sizes.len() == 1 {
        if let Some((width, height)) = sizes.first() {
            println!("  page size:     {width}x{height} pt (uniform)");
        }
    } else {
        println!("  page sizes:    {} distinct", sizes.len());
        for (width, height) in sizes.iter().take(5) {
            println!("                 {width}x{height} pt");
        }
        if sizes.len() > 5 {
            println!("                 ... and {} more", sizes.len() - 5);
        }
    }

    let first_screen = layout.visible_pages(0.0, SAMPLE_VIEWPORT_PT);
    println!(
        "  first {SAMPLE_VIEWPORT_PT:.0} pt: pages {}..{} would rasterize",
        first_screen.start, first_screen.end
    );

    ExitCode::SUCCESS
}

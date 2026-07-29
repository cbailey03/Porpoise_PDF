//! `porpoise` — command line entry point.
//!
//! At M1 this can describe a document and rasterize a page to a PNG. The window
//! arrives at M2; see `docs/goal-1-plan.md`, section 4.

use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use porpoise_doc::Document;
use porpoise_render::{
    HayroRenderer, RenderError, RenderLimits, RenderRequest, render_with_timeout,
};
use porpoise_view::ScrollLayout;

/// Wraps a [`RenderError`] so the message leads with the page number the user
/// actually typed.
///
/// The renderer works in zero-based indices and says so, which is right for a
/// library. But a CLI that accepts `--page 1` and then reports a failure on
/// "page index 0" invites the reader to think they hit the wrong page.
#[derive(Debug, thiserror::Error)]
#[error("could not render page {page} of {file}")]
struct RenderFailed {
    page: usize,
    file: String,
    #[source]
    source: RenderError,
}

/// Gap between pages in the scrolling column, in PDF points.
const PAGE_GAP_PT: f64 = 12.0;

/// Height of the notional viewport used to illustrate the visible set.
const SAMPLE_VIEWPORT_PT: f64 = 800.0;

/// PDF's native resolution: one point is 1/72 inch.
const POINTS_PER_INCH: f32 = 72.0;

#[derive(Parser)]
#[command(
    name = "porpoise",
    version,
    about = "A PDF viewer and editor with no C PDF library in the binary"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Report page count, page sizes, and the scroll layout for a document.
    Info(InfoArgs),
    /// Rasterize a single page to a PNG.
    Render(RenderArgs),
}

#[derive(Args)]
struct InfoArgs {
    /// The PDF to inspect.
    file: PathBuf,
}

#[derive(Args)]
struct RenderArgs {
    /// The PDF to rasterize.
    file: PathBuf,

    /// Page number, counting from 1.
    #[arg(short, long, default_value_t = 1)]
    page: usize,

    /// Scale factor applied to the page. 1.0 renders at 72 DPI.
    #[arg(short, long, default_value_t = 1.0, conflicts_with = "dpi")]
    scale: f32,

    /// Target resolution in DPI, as a friendlier alternative to --scale.
    #[arg(long)]
    dpi: Option<f32>,

    /// Where to write the PNG.
    #[arg(short, long)]
    output: PathBuf,

    /// Give up on the page after this many milliseconds.
    #[arg(long, default_value_t = 10_000)]
    timeout_ms: u64,

    /// Refuse to rasterize more than this many pixels. Defaults to 64 megapixels.
    #[arg(long)]
    max_pixels: Option<u64>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    let outcome = match cli.command {
        Command::Info(args) => run_info(&args),
        Command::Render(args) => run_render(&args),
    };

    if let Err(error) = outcome {
        eprintln!("error: {error}");
        let mut cause = error.source();
        while let Some(current) = cause {
            eprintln!("  caused by: {current}");
            cause = current.source();
        }
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

fn run_info(args: &InfoArgs) -> Result<(), Box<dyn Error>> {
    let document = Document::open(&args.file)?;
    let layout = ScrollLayout::vertical(document.geometry(), PAGE_GAP_PT);

    println!("{}", args.file.display());
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

    if let [(width, height)] = sizes.as_slice() {
        println!("  page size:     {width}x{height} pt (uniform)");
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

    Ok(())
}

fn run_render(args: &RenderArgs) -> Result<(), Box<dyn Error>> {
    // Page numbers are 1-based for humans and 0-based internally. Convert once,
    // here, so the boundary is in one obvious place.
    if args.page == 0 {
        return Err("page numbers start at 1".into());
    }
    let page_index = args.page - 1;

    let scale = match args.dpi {
        Some(dpi) if dpi.is_finite() && dpi > 0.0 => dpi / POINTS_PER_INCH,
        Some(dpi) => return Err(format!("--dpi must be a positive number, got {dpi}").into()),
        None if args.scale.is_finite() && args.scale > 0.0 => args.scale,
        None => {
            return Err(format!("--scale must be a positive number, got {}", args.scale).into());
        }
    };

    let document = Arc::new(Document::open(&args.file)?);

    // Check this before rendering so the message can name the real page count,
    // which is more useful than the renderer's index-out-of-range error.
    if page_index >= document.page_count() {
        return Err(format!(
            "{} has {} page(s), so page {} does not exist",
            args.file.display(),
            document.page_count(),
            args.page
        )
        .into());
    }

    let limits = RenderLimits {
        max_total_pixels: args
            .max_pixels
            .unwrap_or(RenderLimits::DEFAULT_MAX_TOTAL_PIXELS),
        ..RenderLimits::default()
    };

    let started = Instant::now();
    let page = render_with_timeout(
        HayroRenderer::with_limits(limits),
        Arc::clone(&document),
        RenderRequest { page_index, scale },
        Duration::from_millis(args.timeout_ms),
    )
    .map_err(|source| RenderFailed {
        page: args.page,
        file: args.file.display().to_string(),
        source,
    })?;
    let render_time = started.elapsed();

    let png = page.encode_png()?;
    std::fs::write(&args.output, &png)?;

    println!(
        "wrote {} — {}x{} px, {} KB, rasterized in {} ms",
        args.output.display(),
        page.width,
        page.height,
        png.len() / 1024,
        render_time.as_millis()
    );

    Ok(())
}

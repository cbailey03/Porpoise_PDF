//! `porpoise` — command line entry point.
//!
//! Three ways in: a bare file path opens the viewer, `info` describes a document,
//! and `render` rasterizes one page to a PNG. See `docs/goal-1-plan.md`,
//! section 4.

mod command;
mod control;
mod devtools;
mod protocol;
mod viewer;

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand};
use porpoise_doc::Document;
use porpoise_render::{
    HayroRenderer, RenderError, RenderLimits, RenderRequest, render_with_timeout,
};
use porpoise_view::{PageNumber, ScrollLayout};
use tracing::level_filters::LevelFilter;

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
    about = "A PDF viewer and editor with no C PDF library in the binary",
    // So `porpoise file.pdf` opens the viewer while `porpoise info file.pdf`
    // still reaches the subcommand. Makes the binary usable as a file handler.
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// A PDF to open in the viewer.
    file: Option<PathBuf>,

    /// Open scrolled to this page, counting from 1.
    #[arg(long, requires = "file")]
    start_page: Option<usize>,

    /// Development aid: capture the window to this PNG and exit immediately.
    #[arg(long, requires = "file", hide = true)]
    screenshot: Option<PathBuf>,

    /// Development aid: scroll the whole document over this many frames,
    /// report frame-time percentiles, and exit.
    #[arg(long, requires = "file", hide = true)]
    scroll_benchmark: Option<u32>,

    /// Development aid: report how long until the first page is on screen,
    /// then exit.
    #[arg(long, requires = "file", hide = true)]
    time_to_first_page: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Report page count, page sizes, and the scroll layout for a document.
    Info(InfoArgs),
    /// Rasterize a single page to a PNG.
    Render(RenderArgs),
    /// Open a window driven by commands on stdin, reporting on stdout.
    Serve(ServeArgs),
}

#[derive(Args)]
struct InfoArgs {
    /// The PDF to inspect.
    file: PathBuf,
}

#[derive(Args)]
struct ServeArgs {
    /// A PDF to open immediately. Optional — send an `open` command instead.
    file: Option<PathBuf>,

    /// Open scrolled to this page, counting from 1.
    #[arg(long, requires = "file")]
    start_page: Option<usize>,
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
    // Taken before anything else so "time to first page" includes argument
    // parsing, file reading and window creation — everything the user waits for.
    let launched = Instant::now();
    init_tracing();
    let cli = Cli::parse();

    let outcome = match cli.command {
        Some(Command::Info(args)) => run_info(&args),
        Some(Command::Render(args)) => run_render(&args),
        Some(Command::Serve(args)) => run_serve(&args),
        None => run_viewer(
            cli.file.as_deref(),
            cli.start_page,
            cli.screenshot.as_deref(),
            cli.scroll_benchmark,
            cli.time_to_first_page.then_some(launched),
        ),
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

/// Sends `tracing` output to stderr.
///
/// Without a subscriber, every `warn!` in the render pipeline is discarded —
/// including the one reporting that the interpreter panicked on a page, which for
/// this project is the single most important diagnostic there is. stderr rather
/// than stdout so diagnostics never mix with `info` and `render` output.
fn init_tracing() {
    let level = log_level(std::env::var("RUST_LOG").ok().as_deref());
    // A second call would fail, and there is only one call site, so the result is
    // deliberately ignored rather than unwrapped.
    drop(
        tracing_subscriber::fmt()
            .with_max_level(level)
            .with_writer(std::io::stderr)
            .try_init(),
    );
}

/// Maps a `RUST_LOG` value onto a level, defaulting to warnings only.
///
/// Deliberately not `tracing-subscriber`'s `EnvFilter`, which supports per-target
/// directives but pulls in regex machinery to parse them. A bare level covers what
/// this binary can actually emit; per-target filtering can join when there are
/// enough targets to filter between.
fn log_level(setting: Option<&str>) -> LevelFilter {
    match setting
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "trace" => LevelFilter::TRACE,
        "debug" => LevelFilter::DEBUG,
        "info" => LevelFilter::INFO,
        "error" => LevelFilter::ERROR,
        "off" | "none" => LevelFilter::OFF,
        // Anything else — unset, or a per-target directive we do not parse —
        // leaves the default. Staying at `warn` on an unrecognized value is safer
        // than going silent on a typo.
        _ => LevelFilter::WARN,
    }
}

/// Validates a page number typed on the command line against the document.
///
/// Returns a [`PageNumber`] rather than an index: callers that need to index a
/// collection say so with [`PageNumber::index`], which keeps the two conventions
/// distinguishable all the way down. See `porpoise-view`'s crate docs.
fn checked_page(page: usize, page_count: usize, file: &Path) -> Result<PageNumber, String> {
    let Some(page) = PageNumber::new(page) else {
        return Err("page numbers start at 1".to_owned());
    };
    if page.get() > page_count {
        return Err(format!(
            "{} has {page_count} page(s), so page {page} does not exist",
            file.display()
        ));
    }
    Ok(page)
}

/// The scale to rasterize at, from the mutually exclusive `--dpi` and `--scale`.
///
/// clap enforces that they are not combined; this validates the one that was
/// given. Non-finite and non-positive values are rejected here rather than
/// reaching the renderer, so the message names the flag the user typed.
fn resolve_scale(dpi: Option<f32>, scale: f32) -> Result<f32, String> {
    match dpi {
        Some(dpi) if dpi.is_finite() && dpi > 0.0 => Ok(dpi / POINTS_PER_INCH),
        Some(dpi) => Err(format!("--dpi must be a positive number, got {dpi}")),
        None if scale.is_finite() && scale > 0.0 => Ok(scale),
        None => Err(format!("--scale must be a positive number, got {scale}")),
    }
}

fn run_viewer(
    file: Option<&Path>,
    start_page: Option<usize>,
    screenshot: Option<&Path>,
    scroll_benchmark: Option<u32>,
    report_first_page_from: Option<Instant>,
) -> Result<(), Box<dyn Error>> {
    // There is no file dialog yet, so with no path there is nothing to show.
    // Opening an empty window would be worse than saying so.
    let Some(file) = file else {
        return Err("no file given — try `porpoise <file.pdf>`, or `porpoise --help`".into());
    };

    let document = Document::open(file)?;

    let start_page = match start_page {
        Some(page) => Some(checked_page(page, document.page_count(), file)?),
        None => None,
    };

    viewer::run(viewer::ViewerOptions {
        document: Some((file.to_path_buf(), document)),
        start_page,
        control: None,
        devtools: viewer::DevOptions {
            screenshot: screenshot.map(Path::to_path_buf),
            benchmark_frames: scroll_benchmark,
            report_first_page_from,
        },
    })
}

/// Opens a window driven by commands on stdin.
///
/// The document is optional: an agent can send `open` instead. Diagnostics already
/// go to stderr, so stdout carries nothing but the protocol.
fn run_serve(args: &ServeArgs) -> Result<(), Box<dyn Error>> {
    let document = match &args.file {
        Some(file) => {
            let document = Document::open(file)?;
            Some((file.clone(), document))
        }
        None => None,
    };

    let start_page = match (args.start_page, &document) {
        (Some(page), Some((file, document))) => {
            Some(checked_page(page, document.page_count(), file)?)
        }
        // clap's `requires = "file"` makes this unreachable, but returning rather
        // than panicking keeps the invariant local.
        (Some(_), None) => return Err("--start-page needs a file".into()),
        (None, _) => None,
    };

    viewer::run(viewer::ViewerOptions {
        document,
        start_page,
        control: Some(control::Control::stdio()),
        devtools: viewer::DevOptions::default(),
    })
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

    // Printed as page numbers, not indices. This used to read "pages 0..1", which
    // is an index range shown to a person -- exactly the mixed convention that made
    // us settle on one-based everywhere visible.
    let first_screen = layout.visible_pages(0.0, SAMPLE_VIEWPORT_PT);
    match (first_screen.start, first_screen.end) {
        (_, 0) => println!("  first {SAMPLE_VIEWPORT_PT:.0} pt: nothing to rasterize"),
        (start, end) => println!(
            "  first {SAMPLE_VIEWPORT_PT:.0} pt: pages {} to {} would rasterize",
            PageNumber::from_index(start),
            PageNumber::from_index(end - 1)
        ),
    }

    Ok(())
}

fn run_render(args: &RenderArgs) -> Result<(), Box<dyn Error>> {
    let scale = resolve_scale(args.dpi, args.scale)?;
    let document = Arc::new(Document::open(&args.file)?);
    // Checked here rather than in the renderer so the message can name the real
    // page count, which is more useful than an index-out-of-range error.
    let page_index = checked_page(args.page, document.page_count(), &args.file)?.index();

    let limits = RenderLimits {
        max_total_pixels: args
            .max_pixels
            .unwrap_or(RenderLimits::DEFAULT_MAX_TOTAL_PIXELS),
        ..RenderLimits::default()
    };

    let started = Instant::now();
    let page = render_with_timeout(
        HayroRenderer::new().with_limits(limits),
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

#[cfg(test)]
mod tests {
    use super::*;

    use clap::CommandFactory;

    fn doc() -> &'static Path {
        Path::new("report.pdf")
    }

    // --- Page numbering ------------------------------------------------------

    #[test]
    fn page_one_is_index_zero() {
        assert_eq!(checked_page(1, 10, doc()).map(PageNumber::index), Ok(0));
        assert_eq!(checked_page(10, 10, doc()).map(PageNumber::index), Ok(9));
    }

    #[test]
    fn page_zero_is_rejected_because_humans_count_from_one() {
        let error = checked_page(0, 10, doc()).expect_err("page 0 does not exist");
        assert!(error.contains("start at 1"), "unhelpful: {error}");
    }

    #[test]
    fn a_page_past_the_end_names_the_real_page_count() {
        // The renderer's own out-of-range error talks about indices, which reads as
        // an off-by-one to someone who typed a 1-based number.
        let error = checked_page(11, 10, doc()).expect_err("page 11 of 10");
        assert!(error.contains("10 page(s)"), "unhelpful: {error}");
        assert!(error.contains("page 11"), "unhelpful: {error}");
        assert!(error.contains("report.pdf"), "unhelpful: {error}");
    }

    #[test]
    fn an_empty_document_rejects_every_page_number() {
        assert!(checked_page(1, 0, doc()).is_err());
    }

    // --- Scale resolution ----------------------------------------------------

    #[test]
    fn scale_one_is_seventy_two_dpi() {
        assert_eq!(resolve_scale(None, 1.0), Ok(1.0));
        assert_eq!(resolve_scale(Some(72.0), 1.0), Ok(1.0));
    }

    #[test]
    fn dpi_converts_through_points_per_inch() {
        assert_eq!(resolve_scale(Some(144.0), 1.0), Ok(2.0));
        assert_eq!(resolve_scale(Some(36.0), 1.0), Ok(0.5));
    }

    #[test]
    fn dpi_overrides_the_scale_default() {
        // clap forbids passing both, but `scale` still carries its default value,
        // so `dpi` has to win rather than being silently ignored.
        assert_eq!(resolve_scale(Some(150.0), 1.0), Ok(150.0 / 72.0));
    }

    #[test]
    fn a_degenerate_dpi_names_the_dpi_flag() {
        for bad in [0.0, -10.0, f32::NAN, f32::INFINITY] {
            let error =
                resolve_scale(Some(bad), 1.0).expect_err("a degenerate dpi must be refused");
            assert!(error.contains("--dpi"), "{bad} gave: {error}");
        }
    }

    #[test]
    fn a_degenerate_scale_names_the_scale_flag() {
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let error = resolve_scale(None, bad).expect_err("a degenerate scale must be refused");
            assert!(error.contains("--scale"), "{bad} gave: {error}");
        }
    }

    // --- Log level -----------------------------------------------------------

    #[test]
    fn logging_defaults_to_warnings_only() {
        // The default has to include WARN: every diagnostic this binary emits is a
        // warning, so a stricter default would make the subscriber pointless.
        assert_eq!(log_level(None), LevelFilter::WARN);
        assert_eq!(log_level(Some("")), LevelFilter::WARN);
    }

    #[test]
    fn rust_log_selects_a_level_case_insensitively() {
        assert_eq!(log_level(Some("debug")), LevelFilter::DEBUG);
        assert_eq!(log_level(Some("DEBUG")), LevelFilter::DEBUG);
        assert_eq!(log_level(Some("  Info  ")), LevelFilter::INFO);
        assert_eq!(log_level(Some("trace")), LevelFilter::TRACE);
        assert_eq!(log_level(Some("off")), LevelFilter::OFF);
    }

    #[test]
    fn an_unparsed_directive_stays_at_the_default_rather_than_going_silent() {
        // A per-target directive is valid `RUST_LOG` that we do not parse. Falling
        // back to `warn` keeps diagnostics; falling back to `off` would lose them.
        assert_eq!(log_level(Some("porpoise_render=debug")), LevelFilter::WARN);
        assert_eq!(log_level(Some("nonsense")), LevelFilter::WARN);
    }

    // --- CLI wiring ----------------------------------------------------------

    #[test]
    fn the_cli_definition_is_internally_consistent() {
        // Catches contradictory `requires`/`conflicts_with` wiring, which otherwise
        // only surfaces as a panic the first time a user passes that flag.
        Cli::command().debug_assert();
    }

    #[test]
    fn a_bare_path_opens_the_viewer_rather_than_needing_a_subcommand() {
        let cli = Cli::try_parse_from(["porpoise", "file.pdf"]).expect("should parse");
        assert_eq!(cli.file.as_deref(), Some(Path::new("file.pdf")));
        assert!(cli.command.is_none());
    }

    #[test]
    fn dpi_and_scale_cannot_be_combined() {
        let result = Cli::try_parse_from([
            "porpoise", "render", "f.pdf", "-o", "out.png", "--dpi", "150", "--scale", "2",
        ]);
        assert!(result.is_err(), "--dpi and --scale must conflict");
    }

    #[test]
    fn viewer_flags_require_a_file() {
        // `--start-page 3` with no path would otherwise parse and then fail late
        // with a less specific message.
        assert!(Cli::try_parse_from(["porpoise", "--start-page", "3"]).is_err());
    }
}

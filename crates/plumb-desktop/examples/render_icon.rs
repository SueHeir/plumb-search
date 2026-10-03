//! Renders the app icon's SVG source to the square PNG that `cargo tauri
//! icon` turns into every platform's icons. From `crates/plumb-desktop`:
//!
//! ```sh
//! cargo run --example render_icon   # app-icon.svg -> app-icon.png, 1024x1024
//! cargo tauri icon app-icon.png     # -> icons/
//! ```
//!
//! Optional arguments: `[SVG] [PNG] [SIZE]`. The default paths are next to
//! this crate's Cargo.toml, wherever the command runs from.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, ensure, Context, Result};
use resvg::{tiny_skia, usvg};

const DEFAULT_SIZE: u32 = 1024;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut args = std::env::args_os().skip(1);
    let svg = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| crate_dir.join("app-icon.svg"));
    let png = args
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| crate_dir.join("app-icon.png"));
    let size = match args.next() {
        Some(arg) => match arg.to_str().and_then(|s| s.parse::<u32>().ok()) {
            Some(size) if size > 0 => size,
            _ => bail!("SIZE must be a positive whole number of pixels, not {arg:?}"),
        },
        None => DEFAULT_SIZE,
    };
    ensure!(
        args.next().is_none(),
        "usage: render_icon [SVG] [PNG] [SIZE]"
    );

    render(&svg, &png, size)?;
    println!("wrote {} ({size}x{size})", png.display());
    Ok(())
}

/// Draws `svg` scaled to fill a `size` x `size` canvas and saves it as `png`.
fn render(svg: &Path, png: &Path, size: u32) -> Result<()> {
    let data = std::fs::read(svg).with_context(|| format!("reading {}", svg.display()))?;
    let tree = usvg::Tree::from_data(&data, &usvg::Options::default())
        .with_context(|| format!("parsing {}", svg.display()))?;
    let (width, height) = (tree.size().width(), tree.size().height());
    ensure!(
        width == height,
        "{} is {width}x{height}; app icons must be square",
        svg.display()
    );

    let mut pixmap = tiny_skia::Pixmap::new(size, size).context("allocating the image")?;
    let scale = size as f32 / width;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap
        .save_png(png)
        .with_context(|| format!("writing {}", png.display()))
}

//! Turning a site's icon file into a small PNG for results pages.
//!
//! Icons come from sites, so they are untrusted: decoding is limited in
//! size and memory, and what is kept is a fresh PNG drawn from the decoded
//! pixels, never the site's own bytes. Pages can then show it inline
//! without the visitor's browser ever asking the site for anything.

use std::io::Cursor;

use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, ImageReader, Limits, RgbaImage};

/// The width and height of the icons [`normalize_icon`] makes, in pixels:
/// sharp at 16 pixels on a high-density screen.
pub const ICON_SIZE: u32 = 32;

/// Icons wider or taller than this are not decoded.
const MAX_SOURCE_SIDE: u32 = 1024;

/// Memory the decoder may take for one icon.
const MAX_DECODE_BYTES: u64 = 32 * 1024 * 1024;

/// Images narrower or shorter than this are spacers, not icons.
const MIN_SOURCE_SIDE: u32 = 8;

/// The icon in `bytes` (PNG, ICO, JPEG, GIF, WebP or BMP) scaled to fit an
/// [`ICON_SIZE`] square, centered on a transparent background, as a PNG.
/// `None` when `bytes` is not such an image, is too large or too small to
/// be an icon, or shows nothing (every pixel transparent).
pub fn normalize_icon(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let format = reader.format()?;
    let known = [
        ImageFormat::Png,
        ImageFormat::Ico,
        ImageFormat::Jpeg,
        ImageFormat::Gif,
        ImageFormat::WebP,
        ImageFormat::Bmp,
    ];
    if !known.contains(&format) {
        return None;
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SOURCE_SIDE);
    limits.max_image_height = Some(MAX_SOURCE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let image = reader.decode().ok()?;
    if image.width() < MIN_SOURCE_SIDE || image.height() < MIN_SOURCE_SIDE {
        return None;
    }
    let scaled = image
        .resize(ICON_SIZE, ICON_SIZE, FilterType::Lanczos3)
        .to_rgba8();
    if scaled.pixels().all(|pixel| pixel[3] == 0) {
        return None;
    }
    let mut square = RgbaImage::new(ICON_SIZE, ICON_SIZE);
    let x = (ICON_SIZE - scaled.width()) / 2;
    let y = (ICON_SIZE - scaled.height()) / 2;
    image::imageops::overlay(&mut square, &scaled, x.into(), y.into());
    let mut png = Vec::new();
    DynamicImage::ImageRgba8(square)
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .ok()?;
    Some(png)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn png(width: u32, height: u32, alpha: u8) -> Vec<u8> {
        let image = RgbaImage::from_pixel(width, height, image::Rgba([200, 30, 30, alpha]));
        let mut bytes = Vec::new();
        DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    fn size_of(png: &[u8]) -> (u32, u32) {
        let image = image::load_from_memory(png).unwrap();
        (image.width(), image.height())
    }

    #[test]
    fn icons_become_small_square_pngs() {
        for (w, h) in [(16, 16), (64, 64), (180, 180), (48, 24)] {
            let icon = normalize_icon(&png(w, h, 255)).unwrap();
            assert!(icon.starts_with(b"\x89PNG"));
            assert_eq!(size_of(&icon), (ICON_SIZE, ICON_SIZE));
        }
    }

    #[test]
    fn a_wide_icon_is_centered_on_a_clear_background() {
        let icon = image::load_from_memory(&normalize_icon(&png(64, 32, 255)).unwrap())
            .unwrap()
            .to_rgba8();
        assert_eq!(icon.get_pixel(16, 0)[3], 0, "the top stays clear");
        assert_eq!(icon.get_pixel(16, 16)[3], 255, "the middle has the icon");
    }

    #[test]
    fn icon_files_in_other_formats_are_read() {
        let image =
            DynamicImage::ImageRgba8(RgbaImage::from_pixel(32, 32, image::Rgba([0, 0, 255, 255])));
        let mut ico = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut ico), ImageFormat::Ico)
            .unwrap();
        assert!(normalize_icon(&ico).is_some());
    }

    #[test]
    fn things_that_are_not_icons_are_refused() {
        assert!(normalize_icon(b"").is_none());
        assert!(normalize_icon(b"<!doctype html><title>Not found</title>").is_none());
        assert!(normalize_icon(b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>").is_none());
        assert!(normalize_icon(&png(1, 1, 255)).is_none(), "a spacer");
        assert!(normalize_icon(&png(32, 32, 0)).is_none(), "nothing to see");
        assert!(normalize_icon(&png(2000, 16, 255)).is_none(), "too wide");
        let mut cut = png(64, 64, 255);
        cut.truncate(cut.len() / 2);
        assert!(normalize_icon(&cut).is_none());
    }
}

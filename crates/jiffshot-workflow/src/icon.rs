//! Generate the program icon and PNGs from the shared SVG artwork.

use std::io::{self, ErrorKind};

use resvg::usvg::{Options, Tree};
use tiny_skia::{Pixmap, Transform};

const LOGO_SVG: &str = include_str!("../../../assets/jiffshot.svg");
const MAX_PNG_SIZE: u32 = 4096;
const TRAY_LOGO_SCALE: f32 = 1.4;

#[derive(Clone, Copy)]
enum IconStyle {
    Program,
    Tray,
}

fn logo_tree() -> io::Result<Tree> {
    Tree::from_str(LOGO_SVG, &Options::default()).map_err(|error| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("invalid embedded logo SVG: {error}"),
        )
    })
}

fn render_at_scale(tree: &Tree, pixmap: &mut Pixmap, scale: f32) {
    let x = (pixmap.width() as f32 - tree.size().width() * scale) / 2.0;
    let y = (pixmap.height() as f32 - tree.size().height() * scale) / 2.0;
    let transform = Transform::from_row(scale, 0.0, 0.0, scale, x, y);
    resvg::render(tree, transform, &mut pixmap.as_mut());
}

fn render_png(tree: &Tree, size: u32, style: IconStyle) -> io::Result<Vec<u8>> {
    if !(1..=MAX_PNG_SIZE).contains(&size) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("logo size must be between 1 and {MAX_PNG_SIZE} pixels"),
        ));
    }

    let mut pixmap = Pixmap::new(size, size)
        .ok_or_else(|| io::Error::other(format!("could not allocate a {size}x{size} logo")))?;
    let scale = size as f32 / tree.size().width().max(tree.size().height());
    match style {
        IconStyle::Program => render_at_scale(tree, &mut pixmap, scale),
        IconStyle::Tray => {
            let mut silhouette = Pixmap::new(size, size).ok_or_else(|| {
                io::Error::other(format!("could not allocate a {size}x{size} mask"))
            })?;
            render_at_scale(tree, &mut silhouette, scale);
            render_at_scale(tree, &mut pixmap, scale * TRAY_LOGO_SCALE);
            // Enlarge the artwork without losing the original rounded silhouette.
            for (pixel, mask) in pixmap
                .data_mut()
                .as_chunks_mut::<4>()
                .0
                .iter_mut()
                .zip(silhouette.data().as_chunks::<4>().0)
            {
                let alpha = u16::from(mask[3]);
                for channel in pixel {
                    *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
                }
            }
        }
    }
    pixmap
        .encode_png()
        .map_err(|error| io::Error::other(format!("could not encode logo PNG: {error}")))
}

pub(crate) fn png(size: u32) -> io::Result<Vec<u8>> {
    render_png(&logo_tree()?, size, IconStyle::Program)
}

pub(crate) fn tray_png(size: u32) -> io::Result<Vec<u8>> {
    render_png(&logo_tree()?, size, IconStyle::Tray)
}

/// The sizes in the `.ico` files, so each display can pick the nearest.
pub(crate) const ICO_SIZES: [u32; 9] = [16, 20, 24, 32, 40, 48, 64, 128, 256];

fn render_ico(style: IconStyle) -> io::Result<Vec<u8>> {
    let tree = logo_tree()?;
    let images: Vec<(u32, Vec<u8>)> = ICO_SIZES
        .iter()
        .map(|&size| render_png(&tree, size, style).map(|png| (size, png)))
        .collect::<io::Result<_>>()?;
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, png) in &images {
        // 256 is written as 0.
        let side = if *size >= 256 { 0 } else { *size as u8 };
        out.extend_from_slice(&[side, side, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(png.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += png.len() as u32;
    }
    for (_, png) in &images {
        out.extend_from_slice(png);
    }
    Ok(out)
}

/// The program icon as a `.ico` file, one PNG per [`ICO_SIZES`].
pub(crate) fn ico() -> io::Result<Vec<u8>> {
    render_ico(IconStyle::Program)
}

/// The enlarged tray icon as a `.ico` file, one PNG per [`ICO_SIZES`].
pub(crate) fn tray_ico() -> io::Result<Vec<u8>> {
    render_ico(IconStyle::Tray)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_purple(pixel: &[u8; 4]) -> bool {
        let alpha = u16::from(pixel[3]);
        alpha >= 128 && u16::from(pixel[1]) * 255 < alpha * 140
    }

    fn foreground_bounds(image: &Pixmap) -> (u32, u32, u32, u32) {
        let size = image.width();
        let mut bounds = (size, size, 0, 0);
        let mut found = false;
        for (index, pixel) in image.data().as_chunks::<4>().0.iter().enumerate() {
            if is_purple(pixel) {
                let x = index as u32 % size;
                let y = index as u32 / size;
                bounds = (
                    bounds.0.min(x),
                    bounds.1.min(y),
                    bounds.2.max(x),
                    bounds.3.max(y),
                );
                found = true;
            }
        }
        assert!(found, "the {size}px icon must contain the purple mark");
        bounds
    }

    fn assert_ico_contains_renders(icon: Vec<u8>, render: fn(u32) -> io::Result<Vec<u8>>) {
        let sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256];
        assert_eq!(&icon[..6], &[0, 0, 1, 0, 9, 0]);
        let mut next_offset = 6 + 16 * sizes.len();
        for (index, size) in sizes.into_iter().enumerate() {
            let entry = 6 + 16 * index;
            let side = if size == 256 { 0 } else { size as u8 };
            assert_eq!(&icon[entry..entry + 4], &[side, side, 0, 0]);
            assert_eq!(&icon[entry + 4..entry + 8], &[1, 0, 32, 0]);
            let length =
                u32::from_le_bytes(icon[entry + 8..entry + 12].try_into().unwrap()) as usize;
            let offset =
                u32::from_le_bytes(icon[entry + 12..entry + 16].try_into().unwrap()) as usize;
            assert_eq!(offset, next_offset);
            assert_eq!(&icon[offset..offset + length], render(size).unwrap());
            next_offset += length;
        }
        assert_eq!(next_offset, icon.len());
    }

    #[test]
    fn png_renders_the_classic_bolt() {
        let image = Pixmap::decode_png(&png(184).unwrap()).unwrap();
        assert_eq!((image.width(), image.height()), (184, 184));
        let pixel = |x: usize, y: usize| &image.data()[(y * 184 + x) * 4..][..4];
        assert_eq!(pixel(0, 0), &[0, 0, 0, 0]);
        assert_eq!(pixel(92, 20), &[0xF5, 0xEA, 0xFA, 0xFF]);
        assert_eq!(pixel(45, 45), &[0x99, 0x33, 0xCC, 0xFF]);
        assert_eq!(pixel(92, 92), &[0x99, 0x33, 0xCC, 0xFF]);
    }

    #[test]
    fn png_rejects_sizes_outside_the_supported_range() {
        for render in [png as fn(u32) -> io::Result<Vec<u8>>, tray_png] {
            for size in [0, MAX_PNG_SIZE + 1, u32::MAX] {
                assert_eq!(render(size).unwrap_err().kind(), ErrorKind::InvalidInput);
            }
            let image = Pixmap::decode_png(&render(1).unwrap()).unwrap();
            assert_eq!((image.width(), image.height()), (1, 1));
        }
    }

    #[test]
    fn tray_mark_is_larger_without_losing_the_rounded_silhouette() {
        let tree = logo_tree().unwrap();
        for size in ICO_SIZES {
            let program = Pixmap::decode_png(&png(size).unwrap()).unwrap();
            let tray = Pixmap::decode_png(&tray_png(size).unwrap()).unwrap();
            let mut uncropped = Pixmap::new(size, size).unwrap();
            let scale = size as f32 / tree.size().width().max(tree.size().height());
            render_at_scale(&tree, &mut uncropped, scale * TRAY_LOGO_SCALE);
            let (old_left, old_top, old_right, old_bottom) = foreground_bounds(&program);
            let (left, top, right, bottom) = foreground_bounds(&tray);
            let width = right - left + 1;
            let height = bottom - top + 1;
            assert!(
                width > old_right - old_left + 1,
                "{size}px tray mark is not wider"
            );
            assert!(
                height > old_bottom - old_top + 1,
                "{size}px tray mark is not taller"
            );
            assert!(
                width * 4 >= size * 3 && height * 4 >= size * 3,
                "{size}px tray mark is too small"
            );
            assert!(
                left > 0 && top > 0 && right < size - 1 && bottom < size - 1,
                "{size}px tray mark is clipped"
            );
            for ((original, enlarged), source) in program
                .data()
                .as_chunks::<4>()
                .0
                .iter()
                .zip(tray.data().as_chunks::<4>().0)
                .zip(uncropped.data().as_chunks::<4>().0)
            {
                assert_eq!(original[3], enlarged[3], "{size}px silhouette changed");
                if is_purple(source) {
                    assert!(
                        original[3] >= 240,
                        "{size}px tray mark overlaps the rounded edge"
                    );
                }
            }
        }
    }

    #[test]
    fn ico_contains_all_nine_svg_renders() {
        assert_ico_contains_renders(ico().unwrap(), png);
    }

    #[test]
    fn tray_ico_contains_all_nine_enlarged_renders() {
        assert_ico_contains_renders(tray_ico().unwrap(), tray_png);
    }

    #[test]
    fn checked_in_icon_matches_the_svg() {
        assert_eq!(
            ico().unwrap(),
            include_bytes!("../../../assets/jiffshot.ico")
        );
    }

    #[test]
    fn checked_in_tray_assets_match_the_svg() {
        assert_eq!(
            tray_ico().unwrap(),
            include_bytes!("../../../assets/jiffshot-tray.ico")
        );
        assert_eq!(
            tray_png(32).unwrap(),
            include_bytes!("../../../assets/jiffshot-tray.png")
        );
    }
}

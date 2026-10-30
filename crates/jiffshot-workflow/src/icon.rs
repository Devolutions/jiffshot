//! Generate the program icon and PNGs from the shared SVG artwork.

use std::io::{self, ErrorKind};

use resvg::usvg::{Options, Tree};
use tiny_skia::{Pixmap, Transform};

const LOGO_SVG: &str = include_str!("../../../assets/jiffshot.svg");
const MAX_PNG_SIZE: u32 = 4096;

fn logo_tree() -> io::Result<Tree> {
    Tree::from_str(LOGO_SVG, &Options::default()).map_err(|error| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("invalid embedded logo SVG: {error}"),
        )
    })
}

fn render_png(tree: &Tree, size: u32) -> io::Result<Vec<u8>> {
    if !(1..=MAX_PNG_SIZE).contains(&size) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("logo size must be between 1 and {MAX_PNG_SIZE} pixels"),
        ));
    }

    let mut pixmap = Pixmap::new(size, size)
        .ok_or_else(|| io::Error::other(format!("could not allocate a {size}x{size} logo")))?;
    let width = tree.size().width();
    let height = tree.size().height();
    let scale = size as f32 / width.max(height);
    let x = (size as f32 - width * scale) / 2.0;
    let y = (size as f32 - height * scale) / 2.0;
    let transform = Transform::from_row(scale, 0.0, 0.0, scale, x, y);
    resvg::render(tree, transform, &mut pixmap.as_mut());
    pixmap
        .encode_png()
        .map_err(|error| io::Error::other(format!("could not encode logo PNG: {error}")))
}

pub(crate) fn png(size: u32) -> io::Result<Vec<u8>> {
    render_png(&logo_tree()?, size)
}

/// The sizes in the program's `.ico`: Explorer, the taskbar and the title
/// bar each pick the nearest.
pub(crate) const ICO_SIZES: [u32; 9] = [16, 20, 24, 32, 40, 48, 64, 128, 256];

/// The icon as a `.ico` file of PNG images, one per [`ICO_SIZES`].
pub(crate) fn ico() -> io::Result<Vec<u8>> {
    let tree = logo_tree()?;
    let images: Vec<(u32, Vec<u8>)> = ICO_SIZES
        .iter()
        .map(|&size| render_png(&tree, size).map(|png| (size, png)))
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

#[cfg(test)]
mod tests {
    use super::*;

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
        for size in [0, MAX_PNG_SIZE + 1, u32::MAX] {
            assert_eq!(png(size).unwrap_err().kind(), ErrorKind::InvalidInput);
        }
        let image = Pixmap::decode_png(&png(1).unwrap()).unwrap();
        assert_eq!((image.width(), image.height()), (1, 1));
    }

    #[test]
    fn ico_contains_all_nine_svg_renders() {
        let icon = ico().unwrap();
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
            assert_eq!(&icon[offset..offset + length], png(size).unwrap());
            next_offset += length;
        }
        assert_eq!(next_offset, icon.len());
    }

    #[test]
    fn checked_in_icon_matches_the_svg() {
        assert_eq!(
            ico().unwrap(),
            include_bytes!("../../../assets/jiffshot.ico")
        );
    }
}

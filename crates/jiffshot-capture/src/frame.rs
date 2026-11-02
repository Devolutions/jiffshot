use crate::Rect;
use std::io::{self, Write};
use std::path::Path;

/// A captured image: top-down BGRX rows (the GDI layout). The fourth byte
/// is ignored and the image is opaque, unless `alpha` is set (menu
/// captures): then it is straight alpha.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub alpha: bool,
}

impl Frame {
    pub fn new(width: u32, height: u32) -> Frame {
        Frame {
            width,
            height,
            pixels: vec![0; width as usize * height as usize * 4],
            alpha: false,
        }
    }

    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }

    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width as i32, self.height as i32)
    }

    pub fn validate(&self) -> io::Result<()> {
        let expected = (self.width as usize).checked_mul(self.height as usize).and_then(|pixels| pixels.checked_mul(4));
        if self.width == 0 || self.height == 0 || expected != Some(self.pixels.len()) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid image dimensions or pixel buffer"));
        }
        Ok(())
    }

    /// Returns `(r, g, b)` at a pixel, or `None` outside the frame.
    pub fn rgb(&self, x: i32, y: i32) -> Option<(u8, u8, u8)> {
        if !self.bounds().contains(x, y) {
            return None;
        }
        let at = y as usize * self.stride() + x as usize * 4;
        let p = &self.pixels[at..at + 4];
        Some((p[2], p[1], p[0]))
    }

    /// Reads a PNG (any bit depth or color type) as an opaque frame.
    pub fn load_png(path: &Path) -> io::Result<Frame> {
        let file = std::io::BufReader::new(std::fs::File::open(path)?);
        let mut decoder = png::Decoder::new(file);
        decoder.set_transformations(
            png::Transformations::normalize_to_color8() | png::Transformations::ALPHA,
        );
        let mut reader = decoder.read_info().map_err(io::Error::other)?;
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).map_err(io::Error::other)?;
        let mut frame = Frame::new(info.width, info.height);
        let channels = info.color_type.samples();
        for (dst, src) in frame
            .pixels
            .chunks_exact_mut(4)
            .zip(buf.chunks_exact(channels))
        {
            let (r, g, b) = if channels >= 3 {
                (src[0], src[1], src[2])
            } else {
                (src[0], src[0], src[0])
            };
            dst.copy_from_slice(&[b, g, r, 255]);
        }
        Ok(frame)
    }

    /// Copies out `r` (frame coordinates), clipped to the frame.
    pub fn crop(&self, r: Rect) -> Option<Frame> {
        let r = r.intersect(self.bounds())?;
        let mut out = Frame::new(r.width as u32, r.height as u32);
        out.alpha = self.alpha;
        let row = r.width as usize * 4;
        for y in 0..r.height as usize {
            let from = (r.y as usize + y) * self.stride() + r.x as usize * 4;
            out.pixels[y * row..(y + 1) * row].copy_from_slice(&self.pixels[from..from + row]);
        }
        Some(out)
    }

    /// Returns a nearest-neighbor copy at the requested size.
    pub fn resized_nearest(&self, width: u32, height: u32) -> Option<Frame> {
        if width == 0 || height == 0 || self.width == 0 || self.height == 0 {
            return None;
        }
        let mut out = Frame::new(width, height);
        out.alpha = self.alpha;
        for y in 0..height as usize {
            let sy = y * self.height as usize / height as usize;
            for x in 0..width as usize {
                let sx = x * self.width as usize / width as usize;
                let src = sy * self.stride() + sx * 4;
                let dst = y * out.stride() + x * 4;
                out.pixels[dst..dst + 4].copy_from_slice(&self.pixels[src..src + 4]);
            }
        }
        Some(out)
    }

    /// Composites a straight-alpha image over this frame, clipping at its edges.
    pub fn composite_over(&mut self, layer: &Frame, (x, y): (i64, i64)) -> io::Result<()> {
        self.validate()?;
        layer.validate()?;
        let right = x.checked_add(i64::from(layer.width))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid image position"))?;
        let bottom = y.checked_add(i64::from(layer.height))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid image position"))?;
        let left = x.max(0);
        let top = y.max(0);
        let right = right.min(i64::from(self.width));
        let bottom = bottom.min(i64::from(self.height));
        for row in top..bottom {
            for col in left..right {
                let source = (((row - y) as usize * layer.width as usize) + (col - x) as usize) * 4;
                let target = ((row as usize * self.width as usize) + col as usize) * 4;
                let source = &layer.pixels[source..source + 4];
                let target = &mut self.pixels[target..target + 4];
                let source_alpha = if layer.alpha { u32::from(source[3]) } else { 255 };
                if source_alpha == 0 {
                    continue;
                }
                let target_alpha = if self.alpha { u32::from(target[3]) } else { 255 };
                let alpha = source_alpha * 255 + target_alpha * (255 - source_alpha);
                for channel in 0..3 {
                    let value = u32::from(source[channel]) * source_alpha * 255
                        + u32::from(target[channel]) * target_alpha * (255 - source_alpha);
                    target[channel] = ((value + alpha / 2) / alpha) as u8;
                }
                if self.alpha {
                    target[3] = ((alpha + 127) / 255) as u8;
                }
            }
        }
        Ok(())
    }

    /// Writes an RGB (or RGBA) PNG. Captures favour encode speed over size:
    /// a 4K screenshot must save in well under a second.
    pub fn encode_png(&self, out: impl Write) -> io::Result<()> {
        self.validate()?;
        let mut encoder = png::Encoder::new(out, self.width, self.height);
        encoder.set_color(if self.alpha {
            png::ColorType::Rgba
        } else {
            png::ColorType::Rgb
        });
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        encoder.set_filter(png::FilterType::Paeth);
        encoder.set_adaptive_filter(png::AdaptiveFilterType::NonAdaptive);
        let mut writer = encoder.write_header().map_err(io::Error::other)?;
        let mut rgb = Vec::with_capacity(self.width as usize * self.height as usize * 4);
        for px in self.pixels.chunks_exact(4) {
            rgb.extend_from_slice(&[px[2], px[1], px[0]]);
            if self.alpha {
                rgb.push(px[3]);
            }
        }
        writer.write_image_data(&rgb).map_err(io::Error::other)?;
        writer.finish().map_err(io::Error::other)
    }

    pub fn encode_png_vec(&self) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.encode_png(&mut bytes)?;
        Ok(bytes)
    }

    pub fn save_png(&self, path: &Path) -> io::Result<()> {
        let file = std::fs::File::create(path)?;
        self.encode_png(io::BufWriter::with_capacity(1 << 20, file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(width: u32, height: u32) -> Frame {
        let mut frame = Frame::new(width, height);
        for (i, px) in frame.pixels.chunks_exact_mut(4).enumerate() {
            let (x, y) = (i as u32 % width, i as u32 / width);
            px.copy_from_slice(&[x as u8, y as u8, 7, 0]);
        }
        frame
    }

    #[test]
    fn crop_clips_and_copies_rows() {
        let frame = gradient(10, 8);
        let cropped = frame.crop(Rect::new(6, 5, 10, 10)).unwrap();
        assert_eq!((cropped.width, cropped.height), (4, 3));
        assert_eq!(cropped.rgb(0, 0), Some((7, 5, 6)));
        assert_eq!(cropped.rgb(3, 2), Some((7, 7, 9)));
        assert!(frame.crop(Rect::new(20, 20, 5, 5)).is_none());
    }

    #[test]
    fn png_round_trips_colors() {
        let frame = gradient(33, 17);
        let bytes = frame.encode_png_vec().unwrap();
        let decoder = png::Decoder::new(bytes.as_slice());
        let mut reader = decoder.read_info().unwrap();
        let mut buffer = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buffer).unwrap();
        assert_eq!((info.width, info.height), (33, 17));
        let at = (4 * 33 + 9) * 3;
        assert_eq!(&buffer[at..at + 3], &[7, 4, 9]);
    }

    #[test]
    fn nearest_resize_preserves_colors_and_alpha() {
        let mut frame = gradient(2, 1);
        frame.alpha = true;
        frame.pixels[3] = 100;
        frame.pixels[7] = 200;
        let enlarged = frame.resized_nearest(4, 2).unwrap();
        assert!(enlarged.alpha);
        assert_eq!(&enlarged.pixels[..16], &[0, 0, 7, 100, 0, 0, 7, 100, 1, 0, 7, 200, 1, 0, 7, 200]);
        assert_eq!(&enlarged.pixels[16..], &enlarged.pixels[..16]);
        assert!(frame.resized_nearest(0, 1).is_none());
    }

    #[test]
    fn composite_uses_straight_alpha_for_opaque_and_transparent_pixels() {
        let mut frame = Frame { width: 3, height: 1, pixels: vec![
            20, 40, 60, 255,
            10, 20, 30, 0,
            0, 0, 255, 128,
        ], alpha: true };
        let layer = Frame { width: 3, height: 1, pixels: vec![
            200, 100, 50, 128,
            200, 100, 50, 128,
            255, 0, 0, 128,
        ], alpha: true };
        frame.composite_over(&layer, (0, 0)).unwrap();
        assert_eq!(frame.pixels, [
            110, 70, 55, 255,
            200, 100, 50, 128,
            170, 0, 85, 192,
        ]);
    }

    #[test]
    fn composite_clips_negative_positions_and_honors_opaque_frames() {
        let mut frame = Frame::new(2, 2);
        let mut layer = Frame::new(2, 2);
        layer.pixels[12..16].copy_from_slice(&[11, 22, 33, 0]);
        frame.composite_over(&layer, (-1, -1)).unwrap();
        assert!(!frame.alpha);
        assert_eq!(frame.rgb(0, 0), Some((33, 22, 11)));
        assert_eq!(frame.rgb(1, 0), Some((0, 0, 0)));
        let before = frame.pixels.clone();
        frame.composite_over(&layer, (20, 20)).unwrap();
        assert_eq!(frame.pixels, before);
        frame.composite_over(&layer, (i64::MIN, i64::MIN)).unwrap();
        assert_eq!(frame.pixels, before);
    }

    #[test]
    fn composite_leaves_transparent_source_pixels_unchanged() {
        let mut frame = Frame { width: 1, height: 1, pixels: vec![11, 22, 33, 40], alpha: true };
        let layer = Frame { width: 1, height: 1, pixels: vec![255, 255, 255, 0], alpha: true };
        frame.composite_over(&layer, (0, 0)).unwrap();
        assert_eq!(frame.pixels, [11, 22, 33, 40]);
    }

    #[test]
    fn composite_rejects_invalid_buffers_and_overflowing_positions() {
        let mut frame = Frame::new(2, 2);
        let mut layer = Frame::new(1, 1);
        layer.pixels.pop();
        assert_eq!(frame.composite_over(&layer, (0, 0)).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(frame.composite_over(&Frame::new(1, 1), (i64::MAX, 0)).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        frame.pixels.pop();
        assert_eq!(frame.composite_over(&Frame::new(1, 1), (0, 0)).unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }
}

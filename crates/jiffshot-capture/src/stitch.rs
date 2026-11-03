//! Joining overlapping frames into one image, for scrolling capture
//! (frames of one area while its content scrolls vertically) and
//! panoramic capture (frames moved in any direction).

use crate::Frame;
use std::collections::HashMap;

/// Pixels at the right edge left out of row matching: scrollbars move.
const SCROLLBAR: usize = 24;

fn valid_frame(frame: &Frame) -> bool {
    frame.width <= i32::MAX as u32
        && frame.height <= i32::MAX as u32
        && frame.validate().is_ok()
}

fn row_hashes(f: &Frame, left: usize, right: usize) -> Vec<u64> {
    (0..f.height as usize)
        .map(|y| {
            let row = &f.pixels[y * f.stride() + left * 4..y * f.stride() + right * 4];
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for (i, b) in row.iter().enumerate() {
                if i % 4 != 3 {
                    h = (h ^ *b as u64).wrapping_mul(0x100_0000_01b3);
                }
            }
            h
        })
        .collect()
}

fn equal_column(a: &Frame, b: &Frame, x: usize) -> bool {
    (0..a.height as usize).all(|y| {
        let start = y * a.stride() + x * 4;
        a.pixels[start..start + 3] == b.pixels[start..start + 3]
    })
}

/// How `next` continues `prev` when the content between a fixed header and
/// footer scrolled up by `dy` rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    pub dy: usize,
    pub header: usize,
    pub footer: usize,
}

/// Finds how far the content scrolled between two frames of the same area.
/// `None` when nothing moved (the end was reached) or no overlap is found.
pub fn vertical_step(prev: &Frame, next: &Frame) -> Option<Step> {
    if !valid_frame(prev)
        || !valid_frame(next)
        || (prev.width, prev.height) != (next.width, next.height)
    {
        return None;
    }
    if prev.pixels == next.pixels {
        return None;
    }
    // Stationary sidebars do not move with the rows being matched.
    let usable = (prev.width as usize).saturating_sub(SCROLLBAR).max(1);
    let cap = usable / 3;
    let left = (0..cap)
        .take_while(|&x| equal_column(prev, next, x))
        .count();
    let right = (0..cap)
        .take_while(|&x| equal_column(prev, next, usable - x - 1))
        .count();
    let (a, b) = (
        row_hashes(prev, left, usable - right),
        row_hashes(next, left, usable - right),
    );
    let h = a.len();
    if a == b {
        return None;
    }
    // Rows that stay put at the top and bottom are fixed headers and footers.
    let cap = h / 3;
    let header = a
        .iter()
        .zip(&b)
        .take_while(|(x, y)| x == y)
        .count()
        .min(cap);
    let footer = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(x, y)| x == y)
        .count()
        .min(cap);
    let (top, bottom) = (header, h - footer);
    if bottom <= top + 4 {
        return None;
    }
    // Vote for offsets with rows that are distinctive (blank rows repeat).
    let mut positions: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, &v) in a.iter().enumerate().take(bottom).skip(top) {
        positions.entry(v).or_default().push(i);
    }
    let mut votes: HashMap<usize, usize> = HashMap::new();
    for (i, v) in b.iter().enumerate().take(bottom).skip(top) {
        if let Some(rows) = positions.get(v) {
            if rows.len() > 6 {
                continue;
            }
            for &j in rows {
                if j > i {
                    *votes.entry(j - i).or_default() += 1;
                }
            }
        }
    }
    let (&dy, &count) = votes
        .iter()
        .max_by_key(|(dy, n)| (**n, std::cmp::Reverse(**dy)))?;
    let overlap = bottom - top - dy;
    let distinctive = (top..bottom - dy)
        .filter(|&i| positions.get(&b[i]).is_some_and(|r| r.len() <= 6))
        .count();
    (count * 2 >= distinctive.min(overlap) && count >= 3).then_some(Step { dy, header, footer })
}

/// Stacks scrolled frames: the first frame, the new rows each later frame
/// adds, and the last frame's footer. Rows can match by accident between
/// two frames (blank lines at the edge), so the fixed header and footer are
/// the bands that stayed put across every step.
pub fn stitch_vertical(frames: &[Frame]) -> Option<Frame> {
    let first = frames.first()?;
    if frames.iter().any(|frame| !valid_frame(frame)) {
        return None;
    }
    let (w, h) = (first.width as usize, first.height as usize);
    let stride = w.checked_mul(4)?;
    let steps: Vec<Step> = frames
        .windows(2)
        .map_while(|p| vertical_step(&p[0], &p[1]))
        .collect();
    let used = &frames[..steps.len() + 1];
    let footer = steps.iter().map(|s| s.footer).min().unwrap_or(0);
    let height = steps.iter().try_fold(h, |height, step| height.checked_add(step.dy))?;
    let output_height = u32::try_from(height).ok()?;
    if output_height > i32::MAX as u32 {
        return None;
    }
    let capacity = stride.checked_mul(height)?;
    let mut rows = Vec::new();
    rows.try_reserve_exact(capacity).ok()?;
    let alpha = used.iter().any(|frame| frame.alpha);
    let mut append = |frame: &Frame, start: usize, end: usize| {
        let before = rows.len();
        rows.extend_from_slice(&frame.pixels[start * stride..end * stride]);
        if alpha && !frame.alpha {
            for pixel in rows[before..].chunks_exact_mut(4) {
                pixel[3] = 255;
            }
        }
    };
    append(first, 0, h - footer);
    for (frame, step) in used[1..].iter().zip(&steps) {
        let end = h - footer;
        let start = end - step.dy;
        append(frame, start, end);
    }
    let last = used.last().unwrap();
    append(last, h - footer, h);
    Some(Frame {
        width: w as u32,
        height: output_height,
        pixels: rows,
        alpha,
    })
}

/// Luma, averaged over `k` x `k` blocks.
fn gray(f: &Frame, k: usize) -> (Vec<u8>, usize, usize) {
    let (w, h) = (f.width as usize / k, f.height as usize / k);
    let mut out = vec![0u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut sum = 0u32;
            for yy in 0..k {
                for xx in 0..k {
                    let i = (y * k + yy) * f.stride() + (x * k + xx) * 4;
                    let p = &f.pixels[i..i + 3];
                    sum += (p[0] as u32 * 29 + p[1] as u32 * 150 + p[2] as u32 * 77) >> 8;
                }
            }
            out[y * w + x] = (sum / (k * k) as u32) as u8;
        }
    }
    (out, w, h)
}

/// Mean absolute difference of `b` placed at (`dx`, `dy`) over `a`, on the
/// overlap (sampled every `step` pixels); `None` when the overlap is small.
fn sad(a: &[u8], b: &[u8], w: usize, h: usize, dx: i32, dy: i32, step: usize) -> Option<f32> {
    let (x0, x1) = (dx.max(0) as usize, (w as i32 + dx.min(0)) as usize);
    let (y0, y1) = (dy.max(0) as usize, (h as i32 + dy.min(0)) as usize);
    if x1 <= x0 + w / 5 || y1 <= y0 + h / 5 {
        return None;
    }
    let (mut sum, mut n) = (0u64, 0u64);
    for y in (y0..y1).step_by(step) {
        let by = (y as i32 - dy) as usize;
        for x in (x0..x1).step_by(step) {
            let bx = (x as i32 - dx) as usize;
            sum += (a[y * w + x] as i32 - b[by * w + bx] as i32).unsigned_abs() as u64;
            n += 1;
        }
    }
    Some(sum as f32 / n.max(1) as f32)
}

/// Where `next` sits relative to `prev` (content moved by the opposite
/// amount), found coarse to fine. `None` when no placement matches well.
pub fn translation(prev: &Frame, next: &Frame) -> Option<(i32, i32)> {
    if !valid_frame(prev)
        || !valid_frame(next)
        || (prev.width, prev.height) != (next.width, next.height)
    {
        return None;
    }
    let k = 4;
    let (a, w, h) = gray(prev, k);
    let (b, _, _) = gray(next, k);
    let (rx, ry) = (w as i32 * 2 / 3, h as i32 * 2 / 3);
    let mut coarse: Vec<(f32, i32, i32)> = Vec::new();
    for dy in -ry..=ry {
        for dx in -rx..=rx {
            if let Some(d) = sad(&a, &b, w, h, dx, dy, 2) {
                // Prefer small moves on ties (flat areas match anywhere).
                coarse.push((d + (dx.abs() + dy.abs()) as f32 * 0.002, dx, dy));
            }
        }
    }
    // Block averages blur sub-block offsets, so refine several candidates.
    coarse.sort_by(|x, y| x.0.total_cmp(&y.0));
    let (af, wf, hf) = gray(prev, 1);
    let (bf, _, _) = gray(next, 1);
    let r = k as i32 + 1;
    let mut fine = (f32::MAX, 0, 0);
    let better = |score: f32, dx: i32, dy: i32, best: (f32, i32, i32)| {
        score < best.0
            || (score == best.0
                && u64::from(dx.unsigned_abs()) + u64::from(dy.unsigned_abs())
                    < u64::from(best.1.unsigned_abs()) + u64::from(best.2.unsigned_abs()))
    };
    for &(_, cx, cy) in coarse.iter().take(12) {
        for dy in cy * k as i32 - r..=cy * k as i32 + r {
            for dx in cx * k as i32 - r..=cx * k as i32 + r {
                if let Some(d) = sad(&af, &bf, wf, hf, dx, dy, 2) {
                    if better(d, dx, dy, fine) {
                        fine = (d, dx, dy);
                    }
                }
            }
        }
    }
    // Sampling every other pixel can tie neighbors; settle at full density.
    let mut exact = (f32::MAX, fine.1, fine.2);
    for dy in fine.2 - 1..=fine.2 + 1 {
        for dx in fine.1 - 1..=fine.1 + 1 {
            if let Some(d) = sad(&af, &bf, wf, hf, dx, dy, 1) {
                if better(d, dx, dy, exact) {
                    exact = (d, dx, dy);
                }
            }
        }
    }
    (exact.0 < 6.0).then_some((exact.1, exact.2))
}

/// Paints frames at their offsets onto one canvas (later frames on top).
pub fn compose(frames: &[(Frame, (i32, i32))]) -> Option<Frame> {
    if frames.iter().any(|(frame, _)| !valid_frame(frame)) {
        return None;
    }
    let x0 = frames.iter().map(|(_, (x, _))| i64::from(*x)).min()?;
    let y0 = frames.iter().map(|(_, (_, y))| i64::from(*y)).min()?;
    let x1 = frames
        .iter()
        .map(|(f, (x, _))| i64::from(*x) + i64::from(f.width))
        .max()?;
    let y1 = frames
        .iter()
        .map(|(f, (_, y))| i64::from(*y) + i64::from(f.height))
        .max()?;
    let width = u32::try_from(x1 - x0).ok()?;
    let height = u32::try_from(y1 - y0).ok()?;
    if width > i32::MAX as u32 || height > i32::MAX as u32 {
        return None;
    }
    let size = (width as usize).checked_mul(height as usize)?.checked_mul(4)?;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(size).ok()?;
    pixels.resize(size, 0);
    let mut out = Frame { width, height, pixels, alpha: true };
    for (f, (x, y)) in frames {
        let left = usize::try_from(i64::from(*x) - x0).ok()?;
        let top = usize::try_from(i64::from(*y) - y0).ok()?;
        if f.alpha {
            out.composite_over(f, (left as i64, top as i64)).ok()?;
            continue;
        }
        for row in 0..f.height as usize {
            let dst = (top + row) * out.stride() + left * 4;
            let src = row * f.stride();
            out.pixels[dst..dst + f.stride()].copy_from_slice(&f.pixels[src..src + f.stride()]);
            for px in out.pixels[dst..dst + f.stride()].chunks_exact_mut(4) {
                px[3] = 255;
            }
        }
    }
    out.alpha = out.pixels.chunks_exact(4).any(|pixel| pixel[3] != 255);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tall "page" with distinctive rows and some blank ones.
    fn page(width: u32, height: u32) -> Frame {
        let mut f = Frame::new(width, height);
        for y in 0..height as usize {
            let blank = y % 37 < 9;
            for x in 0..width as usize {
                let i = y * f.stride() + x * 4;
                // Blocky noise: structured like UI content, never periodic.
                let mut n = ((x / 6) as u32).wrapping_mul(0x9E37_79B9)
                    ^ ((y / 6) as u32).wrapping_mul(0x85EB_CA6B);
                n ^= n >> 15;
                n = n.wrapping_mul(0x2C1B_3C6D);
                let v = if blank { 250 } else { (n >> 24) as u8 };
                f.pixels[i..i + 4].copy_from_slice(&[v, v.wrapping_add(40), (y % 251) as u8, 0]);
            }
        }
        f
    }

    /// The view of `page` scrolled to `top`, with a fixed header and footer.
    fn view(page: &Frame, top: usize, h: usize, header: usize, footer: usize) -> Frame {
        let mut f = Frame::new(page.width, h as u32);
        let s = f.stride();
        for y in 0..h {
            let src = if y < header {
                y
            } else if y >= h - footer {
                page.height as usize - (h - y)
            } else {
                top + y
            };
            f.pixels[y * s..(y + 1) * s].copy_from_slice(&page.pixels[src * s..(src + 1) * s]);
        }
        f
    }

    #[test]
    fn stitches_scrolled_frames_with_fixed_header_and_footer() {
        let p = page(200, 2000);
        let (h, header, footer) = (400, 30, 20);
        let tops = [
            0, 170, 340, 510, 680, 850, 1020, 1190, 1360, 1530, 1580, 1580,
        ];
        let frames: Vec<Frame> = tops
            .iter()
            .map(|&t| view(&p, t, h, header, footer))
            .collect();
        assert_eq!(
            vertical_step(&frames[0], &frames[1]),
            Some(Step {
                dy: 170,
                header,
                footer
            })
        );
        assert_eq!(vertical_step(&frames[10], &frames[11]), None, "end of page");
        let out = stitch_vertical(&frames).unwrap();
        // The header, every content row down to the last view, the footer.
        assert_eq!(out.height as usize, 1580 + h);
        let s = out.stride();
        for y in [0usize, 100, 700, 1500, 1959] {
            assert_eq!(
                &out.pixels[y * s..(y + 1) * s],
                &p.pixels[y * s..(y + 1) * s],
                "row {y}"
            );
        }
        let last = out.height as usize - 1;
        assert_eq!(
            &out.pixels[last * s..],
            &p.pixels[(p.height as usize - 1) * s..],
            "footer"
        );
    }

    #[test]
    fn stitches_scrolled_frames_with_a_fixed_left_sidebar() {
        let p = page(200, 1000);
        let mut frames = [view(&p, 0, 300, 30, 20), view(&p, 120, 300, 30, 20)];
        for frame in &mut frames {
            let stride = frame.stride();
            for y in 0..frame.height as usize {
                for x in 0..12 {
                    let at = y * stride + x * 4;
                    frame.pixels[at..at + 4].copy_from_slice(&[
                        (x * 11 + y * 7) as u8,
                        y as u8,
                        x as u8,
                        0,
                    ]);
                }
            }
        }
        assert_eq!(
            vertical_step(&frames[0], &frames[1]),
            Some(Step {
                dy: 120,
                header: 30,
                footer: 20,
            })
        );
        let out = stitch_vertical(&frames).unwrap();
        assert_eq!((out.width, out.height), (200, 420));
        let offset = 350 * out.stride() + 100 * 4;
        let expected = 350 * p.stride() + 100 * 4;
        assert_eq!(
            &out.pixels[offset..offset + 4],
            &p.pixels[expected..expected + 4]
        );
    }

    #[test]
    fn finds_panoramic_translations() {
        let p = page(900, 700);
        let crop = |x: i32, y: i32| p.crop(crate::Rect::new(x, y, 400, 300)).unwrap();
        let a = crop(100, 100);
        let b = crop(260, 150);
        assert_eq!(translation(&a, &b), Some((160, 50)));
        let c = crop(40, 30);
        assert_eq!(translation(&a, &c), Some((-60, -70)));
        let out = compose(&[(a, (0, 0)), (b, (160, 50))]).unwrap();
        assert_eq!((out.width, out.height), (560, 350));
    }

    #[test]
    fn translation_prefers_no_motion_when_uniform_frames_have_equal_scores() {
        let a = Frame::new(32, 32);
        let mut b = Frame::new(32, 32);
        for pixel in b.pixels.chunks_exact_mut(4) {
            pixel[..3].fill(1);
        }
        assert_eq!(translation(&a, &b), Some((0, 0)));
    }

    #[test]
    fn compose_preserves_source_transparency_and_uncovered_gaps() {
        let mut translucent = Frame::new(1, 1);
        translucent.alpha = true;
        translucent.pixels.copy_from_slice(&[1, 2, 3, 64]);
        let result = compose(&[(translucent, (0, 0))]).unwrap();
        assert!(result.alpha);
        assert_eq!(result.pixels, [1, 2, 3, 64]);

        let left = Frame::new(2, 1);
        let right = Frame::new(2, 1);
        let result = compose(&[(left, (0, 0)), (right, (3, 0))]).unwrap();
        assert!(result.alpha);
        assert_eq!(result.pixels[3], 255);
        assert_eq!(result.pixels[2 * 4 + 3], 0);
        assert_eq!(result.pixels[3 * 4 + 3], 255);
    }

    #[test]
    fn compose_blends_transparent_frames_over_existing_pixels() {
        let mut bottom = Frame::new(2, 1);
        bottom.pixels.copy_from_slice(&[0, 0, 255, 0, 0, 0, 255, 0]);
        let top = Frame {
            width: 2,
            height: 1,
            pixels: vec![255, 0, 0, 128, 99, 99, 99, 0],
            alpha: true,
        };
        let result = compose(&[(bottom, (0, 0)), (top, (0, 0))]).unwrap();
        assert!(!result.alpha);
        assert_eq!(result.rgb(0, 0), Some((127, 0, 128)));
        assert_eq!(result.rgb(1, 0), Some((255, 0, 0)));
    }

    #[test]
    fn vertical_stitch_keeps_transparent_source_pixels() {
        let mut frame = Frame::new(2, 1);
        frame.alpha = true;
        frame.pixels.copy_from_slice(&[1, 2, 3, 64, 4, 5, 6, 0]);
        let result = stitch_vertical(&[frame]).unwrap();
        assert!(result.alpha);
        assert_eq!(result.pixels, [1, 2, 3, 64, 4, 5, 6, 0]);

        let p = page(80, 200);
        let first = view(&p, 0, 60, 4, 4);
        let mut second = view(&p, 20, 60, 4, 4);
        second.alpha = true;
        for pixel in second.pixels.chunks_exact_mut(4) {
            pixel[3] = 64;
        }
        let result = stitch_vertical(&[first, second]).unwrap();
        assert!(result.alpha);
        assert_eq!(result.height, 80);
        assert_eq!(result.pixels[3], 255);
        assert_eq!(result.pixels[59 * result.stride() + 3], 64);
    }

    #[test]
    fn compose_rejects_unrepresentable_extents() {
        let frame = Frame::new(1, 1);
        assert!(compose(&[
            (frame.clone(), (i32::MIN, 0)),
            (frame, (i32::MAX, 0)),
        ])
        .is_none());
    }

    #[test]
    fn stitching_rejects_invalid_frame_buffers() {
        let invalid = Frame {
            width: 2,
            height: 2,
            pixels: vec![],
            alpha: false,
        };
        assert!(stitch_vertical(&[invalid.clone()]).is_none());
        assert!(vertical_step(&invalid, &Frame::new(2, 2)).is_none());
        assert!(translation(&invalid, &Frame::new(2, 2)).is_none());
        assert!(compose(&[(invalid, (0, 0))]).is_none());
    }
}

//! Frame handling for the M1.1 capture baseline: decoding shared-memory
//! buffers into RGBA8, undoing output transforms, and writing a PNG.

use std::path::Path;

use anyhow::{bail, Context as _};
use wayland_client::protocol::{wl_output, wl_shm};

/// A fully decoded frame in upright (logical) pixel order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Tightly packed `width * height * 4` bytes, row-major, non-premultiplied
    /// RGBA.
    pub rgba: Vec<u8>,
}

impl Frame {
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> Self {
        debug_assert_eq!(rgba.len(), (width as usize) * (height as usize) * 4);
        Self {
            width,
            height,
            rgba,
        }
    }

    /// Write the frame to `path` as an 8-bit RGBA PNG, creating parent
    /// directories as needed.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
        let file = std::fs::File::create(path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        let mut encoder = png::Encoder::new(file, self.width, self.height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .with_context(|| format!("failed to write the PNG header for {}", path.display()))?;
        writer
            .write_image_data(&self.rgba)
            .with_context(|| format!("failed to write the PNG pixels to {}", path.display()))?;
        Ok(())
    }
}

/// True when `t` rotates the image by (close to) 90°, swapping the axes.
const fn swaps_axes(t: wl_output::Transform) -> bool {
    matches!(
        t,
        wl_output::Transform::_90
            | wl_output::Transform::_270
            | wl_output::Transform::Flipped90
            | wl_output::Transform::Flipped270
    )
}

/// Map an upright pixel `(x, y)` of an `lw` x `lh` image to its position in a
/// buffer produced by applying `t` to that image.
///
/// Follows the `wl_output.transform` enum: `90` is a 90° counter-clockwise
/// rotation, `flipped` mirrors around the vertical axis, and `flipped_N`
/// mirrors first and then rotates N degrees counter-clockwise.
const fn forward(t: wl_output::Transform, x: u32, y: u32, lw: u32, lh: u32) -> (u32, u32) {
    match t {
        wl_output::Transform::Normal => (x, y),
        wl_output::Transform::_90 => (y, lw - 1 - x),
        wl_output::Transform::_180 => (lw - 1 - x, lh - 1 - y),
        wl_output::Transform::_270 => (lh - 1 - y, x),
        wl_output::Transform::Flipped => (lw - 1 - x, y),
        wl_output::Transform::Flipped90 => (y, x),
        wl_output::Transform::Flipped180 => (x, lh - 1 - y),
        wl_output::Transform::Flipped270 => (lh - 1 - y, lw - 1 - x),
        // Unknown/future transform: behave like `normal` instead of panicking.
        _ => (x, y),
    }
}

/// Undo the transform the compositor applied to the buffer contents.
///
/// Returns the upright pixels plus the upright dimensions (swapped relative to
/// the buffer for the 90°/270° transforms).
///
/// The M1.1 hardware only exercises `transform=normal` (the identity path,
/// verified live on both niri outputs); the other seven cases are covered by
/// round-trip unit tests, not by a rotated physical output.
pub fn untransform(buf: &[u8], bw: u32, bh: u32, t: wl_output::Transform) -> (Vec<u8>, u32, u32) {
    let (ow, oh) = if swaps_axes(t) { (bh, bw) } else { (bw, bh) };
    debug_assert_eq!(buf.len(), (bw as usize) * (bh as usize) * 4);

    let mut out = vec![0u8; (ow as usize) * (oh as usize) * 4];
    for y in 0..oh {
        for x in 0..ow {
            // Each upright pixel is gathered from where `t` would have put it.
            let (sx, sy) = forward(t, x, y, ow, oh);
            let src = ((sy * bw + sx) * 4) as usize;
            let dst = ((y * ow + x) * 4) as usize;
            out[dst..dst + 4].copy_from_slice(&buf[src..src + 4]);
        }
    }
    (out, ow, oh)
}

/// Flip a buffer the compositor marked `Y_INVERT` (rows stored bottom-up).
pub fn flip_rows(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let row_bytes = (width as usize) * 4;
    let rows = height as usize;
    debug_assert_eq!(rgba.len(), row_bytes * rows);

    let mut out = vec![0u8; rgba.len()];
    for (dst_y, src_y) in (0..rows).rev().enumerate() {
        out[dst_y * row_bytes..(dst_y + 1) * row_bytes]
            .copy_from_slice(&rgba[src_y * row_bytes..(src_y + 1) * row_bytes]);
    }
    out
}

/// Decode a shared-memory buffer into tightly packed RGBA8.
///
/// `stride` may exceed `width * 4` when the compositor pads rows.
pub fn decode(
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
    data: &[u8],
) -> anyhow::Result<Vec<u8>> {
    if width == 0 || height == 0 {
        bail!("refusing to decode an empty {width}x{height} frame");
    }
    let w = width as usize;
    let h = height as usize;
    let stride = stride as usize;
    if stride < w * 4 {
        bail!(
            "stride {stride} is smaller than {width} RGBA pixels ({} bytes)",
            w * 4
        );
    }
    let needed = stride
        .checked_mul(h)
        .context("shared-memory buffer size overflows")?;
    if data.len() < needed {
        bail!(
            "shared-memory read returned {} bytes, expected at least {needed}",
            data.len()
        );
    }

    // Convert one little-endian 8888 word to RGBA. The byte layouts come from
    // the `wl_shm.format` documentation.
    let convert: fn([u8; 4]) -> [u8; 4] = match format {
        wl_shm::Format::Argb8888 => |[b, g, r, a]| [r, g, b, a],
        wl_shm::Format::Xrgb8888 => |[b, g, r, _]| [r, g, b, 0xff],
        wl_shm::Format::Xbgr8888 => |[r, g, b, _]| [r, g, b, 0xff],
        wl_shm::Format::Rgba8888 => |[a, b, g, r]| [r, g, b, a],
        other => {
            bail!("unsupported wl_shm format {other:?} (M1.1 supports XRGB/ARGB/XBGR/RGBA 8888)")
        }
    };

    let mut out = vec![0u8; w * h * 4];
    for row in 0..h {
        let src = &data[row * stride..row * stride + w * 4];
        let dst = &mut out[row * w * 4..(row + 1) * w * 4];
        for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
            let word: [u8; 4] = s.try_into().expect("chunks_exact(4) yields 4 bytes");
            d.copy_from_slice(&convert(word));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wl_output::Transform as T;

    /// Place an upright image into a buffer by applying `t` — the constructive
    /// inverse of [`untransform`].
    fn apply_transform(logical: &[u8], lw: u32, lh: u32, t: T) -> (Vec<u8>, u32, u32) {
        let (bw, bh) = if swaps_axes(t) { (lh, lw) } else { (lw, lh) };
        let mut buf = vec![0u8; (bw as usize) * (bh as usize) * 4];
        for y in 0..lh {
            for x in 0..lw {
                let (sx, sy) = forward(t, x, y, lw, lh);
                let src = ((y * lw + x) * 4) as usize;
                let dst = ((sy * bw + sx) * 4) as usize;
                buf[dst..dst + 4].copy_from_slice(&logical[src..src + 4]);
            }
        }
        (buf, bw, bh)
    }

    /// Asymmetric pattern: every pixel is distinguishable in both coordinates.
    fn pattern(w: u32, h: u32) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                v.extend([0x20, x as u8, y as u8, 0xff]);
            }
        }
        v
    }

    const ALL: [T; 8] = [
        T::Normal,
        T::_90,
        T::_180,
        T::_270,
        T::Flipped,
        T::Flipped90,
        T::Flipped180,
        T::Flipped270,
    ];

    #[test]
    fn untransform_roundtrips_every_transform() {
        for t in ALL {
            for (lw, lh) in [(1, 1), (3, 2), (2, 3), (5, 5)] {
                let logical = pattern(lw, lh);
                let (buf, bw, bh) = apply_transform(&logical, lw, lh, t);
                let (out, ow, oh) = untransform(&buf, bw, bh, t);
                assert_eq!((ow, oh), (lw, lh), "dimensions for {t:?} {lw}x{lh}");
                assert_eq!(out, logical, "pixels for {t:?} {lw}x{lh}");
            }
        }
    }

    #[test]
    fn axes_swap_only_for_quarter_turns() {
        // Non-swapping transforms keep the buffer dimensions; the quarter
        // turns (and their flipped forms) transpose them.
        let (bw, bh) = (2u32, 1u32);
        let buf = vec![0u8; (bw * bh * 4) as usize];
        for t in ALL {
            let (_, ow, oh) = untransform(&buf, bw, bh, t);
            let expect = if swaps_axes(t) { (bh, bw) } else { (bw, bh) };
            assert_eq!((ow, oh), expect, "for {t:?}");
        }
    }

    #[test]
    fn transform_90_rotates_counter_clockwise() {
        // Upright 2x1: A on the left, B on the right.
        let logical = [1u8, 0, 0, 255, 2, 0, 0, 255];
        let (buf, bw, bh) = apply_transform(&logical, 2, 1, T::_90);
        assert_eq!((bw, bh), (1, 2));
        // Counter-clockwise puts the right-hand pixel on top.
        assert_eq!(buf, [2, 0, 0, 255, 1, 0, 0, 255]);
    }

    #[test]
    fn decode_xrgb8888_with_stride_padding() {
        // Two pixels with BGRX bytes, 4 bytes of row padding.
        let data = [
            3, 2, 1, 0xAA, // B G R X
            6, 5, 4, 0xBB, //
            0xEE, 0xEE, 0xEE, 0xEE, // padding
        ];
        let out = decode(wl_shm::Format::Xrgb8888, 2, 1, 12, &data).unwrap();
        assert_eq!(out, [1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn decode_argb8888_keeps_alpha() {
        let data = [3, 2, 1, 0x80]; // B G R A
        let out = decode(wl_shm::Format::Argb8888, 1, 1, 4, &data).unwrap();
        assert_eq!(out, [1, 2, 3, 0x80]);
    }

    #[test]
    fn decode_rejects_bad_input() {
        // Too little data for the promised geometry.
        assert!(decode(wl_shm::Format::Xrgb8888, 2, 1, 8, &[0u8; 4]).is_err());
        // Stride below the row's RGBA size.
        assert!(decode(wl_shm::Format::Xrgb8888, 4, 1, 4, &[0u8; 16]).is_err());
        // Empty frame.
        assert!(decode(wl_shm::Format::Xrgb8888, 0, 1, 4, &[]).is_err());
        // Format outside the M1.1 support set.
        assert!(decode(wl_shm::Format::Rgb565, 1, 1, 4, &[0u8; 4]).is_err());
    }

    #[test]
    fn flip_rows_reverses_row_order() {
        let data = [255u8, 0, 0, 255, 0, 0, 255, 255]; // red over blue
        assert_eq!(flip_rows(&data, 1, 2), [0, 0, 255, 255, 255, 0, 0, 255]);
        // A single row is unchanged.
        let row = [9u8, 8, 7, 6];
        assert_eq!(flip_rows(&row, 1, 1), row);
    }

    #[test]
    fn save_writes_png_with_dimensions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/frame.png");
        Frame::new(3, 2, pattern(3, 2))
            .save(&path)
            .expect("png should save");

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "PNG signature");
        let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
        assert_eq!((width, height), (3, 2), "IHDR dimensions");
    }
}

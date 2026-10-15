//! The application icon, drawn in code (AGENTS.md §2: no bundled artwork).
//!
//! Plain `std` on purpose: `apps/fluxcapture/build.rs` includes this file to embed the same icon
//! in the executable's resources (Explorer, pinned shortcuts), while the UI sets it on the window
//! at run time (title bar, taskbar, Alt+Tab).

/// The icon as straight-alpha RGBA, `n`×`n`: a teal ring around a red "record" dot.
pub fn rgba(n: u32) -> Vec<u8> {
    let n = n.clamp(1, 1024);
    // Geometry on a 64-unit grid, scaled to `n` pixels and anti-aliased by pixel coverage.
    let unit = 64.0 / n as f32;
    let centre = (n as f32 - 1.0) / 2.0;
    let disc = |r: f32, d: f32| ((r - d) / unit + 0.5).clamp(0.0, 1.0);
    let mut out = vec![0u8; n as usize * n as usize * 4];
    for y in 0..n {
        for x in 0..n {
            let (dx, dy) = (x as f32 - centre, y as f32 - centre);
            let d = (dx * dx + dy * dy).sqrt() * unit;
            let dot = disc(14.0, d);
            let ring = (disc(30.0, d) - disc(22.0, d)).max(0.0);
            let (c, a) = if dot > 0.0 { ([230u8, 57, 70], dot) } else { ([0u8, 168, 150], ring) };
            let i = (y as usize * n as usize + x as usize) * 4;
            if let Some(px) = out.get_mut(i..i + 4) {
                px.copy_from_slice(&[c[0], c[1], c[2], (a * 255.0).round() as u8]);
            }
        }
    }
    out
}

/// One icon image in the format of an `.ico` entry / `RT_ICON` resource: a 32-bit DIB (BGRA,
/// bottom-up, height doubled) followed by its 1-bit AND mask.
pub fn dib(n: u32) -> Vec<u8> {
    let n = n.clamp(1, 1024);
    let px = rgba(n);
    let mask_stride = n.div_ceil(32) as usize * 4;
    let xor_len = n as usize * n as usize * 4;
    let mask_len = mask_stride * n as usize;
    let mut out = Vec::with_capacity(40 + xor_len + mask_len);
    // BITMAPINFOHEADER
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(n as i32).to_le_bytes());
    out.extend_from_slice(&(2 * n as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&((xor_len + mask_len) as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 16]); // resolution, colours used / important
    let mut mask = vec![0u8; mask_len];
    for row in (0..n as usize).rev() {
        let flipped = n as usize - 1 - row;
        for x in 0..n as usize {
            let i = (row * n as usize + x) * 4;
            let [r, g, b, a] = match px.get(i..i + 4) {
                Some(&[r, g, b, a]) => [r, g, b, a],
                _ => [0; 4],
            };
            out.extend_from_slice(&[b, g, r, a]);
            if a == 0
                && let Some(m) = mask.get_mut(flipped * mask_stride + x / 8)
            {
                *m |= 0x80 >> (x % 8);
            }
        }
    }
    out.extend_from_slice(&mask);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_layout() {
        for n in [16, 20, 24, 32, 48, 256] {
            let px = rgba(n);
            assert_eq!(px.len(), (n * n * 4) as usize);
            let c = ((n / 2) * n + n / 2) as usize * 4;
            assert_eq!(&px[c..c + 4], &[230, 57, 70, 255], "red centre at {n}px");
            assert_eq!(px[3], 0, "transparent corner at {n}px");
            let d = dib(n);
            let mask = n.div_ceil(32) as usize * 4 * n as usize;
            assert_eq!(d.len(), 40 + (n * n * 4) as usize + mask);
            assert_eq!(i32::from_le_bytes([d[8], d[9], d[10], d[11]]), 2 * n as i32);
            // First mask bit = bottom-left corner, transparent.
            assert_eq!(d[40 + (n * n * 4) as usize] & 0x80, 0x80);
        }
        // Hostile sizes are clamped, never panic.
        assert_eq!(rgba(0).len(), 4);
        assert_eq!(rgba(u32::MAX).len(), 1024 * 1024 * 4);
    }
}

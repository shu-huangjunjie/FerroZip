//! The application icon as RGBA pixels. Shared by the window icon and by
//! build.rs, which turns it into the .ico embedded in the executable.

/// Draw the icon at `size`×`size`: rounded blue tile, white folder tab and a zipper.
pub fn draw(size: u32) -> Vec<u8> {
    let s = size as f32;
    let mut px = vec![0u8; (size * size * 4) as usize];
    let radius = s * 0.22;
    let inside_rounded = |x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32| -> f32 {
        // Signed distance-ish coverage for a rounded rectangle, antialiased over 1px.
        let cx = x.clamp(x0 + r, x1 - r);
        let cy = y.clamp(y0 + r, y1 - r);
        let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt() - r;
        (0.5 - d).clamp(0.0, 1.0)
    };
    for yi in 0..size {
        for xi in 0..size {
            let (x, y) = (xi as f32 + 0.5, yi as f32 + 0.5);
            let i = ((yi * size + xi) * 4) as usize;
            let tile = inside_rounded(x, y, s * 0.04, s * 0.04, s * 0.96, s * 0.96, radius);
            if tile <= 0.0 {
                continue;
            }
            // Vertical gradient from #3B8BFF to #1858D6.
            let t = y / s;
            let mut c = [(0x3B as f32 * (1.0 - t) + 0x18 as f32 * t), (0x8B as f32 * (1.0 - t) + 0x58 as f32 * t), (0xFF as f32 * (1.0 - t) + 0xD6 as f32 * t)];
            // White document card.
            let card = inside_rounded(x, y, s * 0.24, s * 0.18, s * 0.76, s * 0.84, s * 0.06);
            // Zipper teeth down the middle, alternating sides.
            let zx = s * 0.5;
            let tooth_h = s * 0.075;
            let k = ((y - s * 0.22) / tooth_h).floor();
            let in_zip_rows = y > s * 0.22 && y < s * 0.62;
            let left = (k as i32) % 2 == 0;
            let tooth = in_zip_rows
                && ((left && x > zx - s * 0.09 && x < zx) || (!left && x >= zx && x < zx + s * 0.09))
                && (y - s * 0.22) % tooth_h < tooth_h * 0.7;
            // Zipper pull.
            let pull = inside_rounded(x, y, zx - s * 0.075, s * 0.62, zx + s * 0.075, s * 0.76, s * 0.03);
            if card > 0.0 {
                let w = [255.0, 255.0, 255.0];
                for ch in 0..3 {
                    c[ch] = c[ch] * (1.0 - card) + w[ch] * card;
                }
                let dark = if tooth { 1.0 } else { pull };
                if dark > 0.0 {
                    let z = [0x1F as f32, 0x5F as f32, 0xD8 as f32];
                    for ch in 0..3 {
                        c[ch] = c[ch] * (1.0 - dark) + z[ch] * dark;
                    }
                }
            }
            px[i] = c[0] as u8;
            px[i + 1] = c[1] as u8;
            px[i + 2] = c[2] as u8;
            px[i + 3] = (tile * 255.0) as u8;
        }
    }
    px
}

/// Minimal .ico writer (32-bit BGRA DIB images, bottom-up, with AND mask),
/// used by the build scripts to embed the icon in the executables.
#[allow(dead_code)]
pub fn write_ico(path: &std::path::Path, sizes: &[u32]) -> std::io::Result<()> {
    let images: Vec<Vec<u8>> = sizes
        .iter()
        .map(|&s| {
            let rgba = draw(s);
            let mut dib = Vec::new();
            dib.extend_from_slice(&40u32.to_le_bytes());
            dib.extend_from_slice(&(s as i32).to_le_bytes());
            dib.extend_from_slice(&((s * 2) as i32).to_le_bytes());
            dib.extend_from_slice(&1u16.to_le_bytes());
            dib.extend_from_slice(&32u16.to_le_bytes());
            dib.extend_from_slice(&[0u8; 24]);
            for y in (0..s).rev() {
                for x in 0..s {
                    let i = ((y * s + x) * 4) as usize;
                    dib.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
                }
            }
            let mask_row = s.div_ceil(32) * 4;
            dib.extend(std::iter::repeat_n(0u8, (mask_row * s) as usize));
            dib
        })
        .collect();
    let mut f = std::fs::File::create(path)?;
    use std::io::Write;
    f.write_all(&[0, 0, 1, 0])?;
    f.write_all(&(sizes.len() as u16).to_le_bytes())?;
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (s, img) in sizes.iter().zip(&images) {
        let dim = if *s >= 256 { 0 } else { *s as u8 };
        f.write_all(&[dim, dim, 0, 0])?;
        f.write_all(&1u16.to_le_bytes())?;
        f.write_all(&32u16.to_le_bytes())?;
        f.write_all(&(img.len() as u32).to_le_bytes())?;
        f.write_all(&offset.to_le_bytes())?;
        offset += img.len() as u32;
    }
    for img in &images {
        f.write_all(img)?;
    }
    Ok(())
}

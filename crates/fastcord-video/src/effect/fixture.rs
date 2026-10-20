//! A rendered person over a textured background, for offline quality tests.
//!
//! The shapes use integer arithmetic on a 256×256 grid so the same scene can
//! be reproduced bit for bit elsewhere (the TensorFlow Lite reference values in
//! `segment.rs` came from the same formulas in Python). At other sizes the
//! scene is stretched over the frame, exactly as the segmenter stretches the
//! frame over its input.

/// `(red, green, blue, person)` at grid position `(i, j)` of 0..256.
fn pixel(i: i64, j: i64) -> ([u8; 3], bool) {
    let (dx, dy) = (i - 128, j - 90);
    if dx * dx * 2500 + dy * dy * 1600 <= 4_000_000 {
        return ([225, 170, 140], true); // head
    }
    let dy = j - 260;
    if j >= 130 && dx * dx * 14_400 + dy * dy * 8100 <= 116_640_000 {
        return ([40, 70, 140], true); // shoulders
    }
    if dx.abs() <= 14 && (120..=140).contains(&j) {
        return ([225, 170, 140], true); // neck
    }
    (
        [
            (96 + (i * 3 + j * 5) % 64) as u8,
            (60 + ((i * 7) ^ (j * 3)) % 80) as u8,
            (100 + ((i / 4 + j / 4) % 2) * 60) as u8,
        ],
        false,
    )
}

fn at(w: usize, h: usize, x: usize, y: usize) -> ([u8; 3], bool) {
    pixel((x * 256 / w) as i64, (y * 256 / h) as i64)
}

/// Planar RGB floats in 0..=1, `3 * w * h`.
pub fn scene_rgb(w: usize, h: usize) -> Vec<f32> {
    let mut out = vec![0.0; 3 * w * h];
    for y in 0..h {
        for x in 0..w {
            let (rgb, _) = at(w, h, x, y);
            for (c, &value) in rgb.iter().enumerate() {
                out[c * w * h + y * w + x] = f32::from(value) / 255.0;
            }
        }
    }
    out
}

/// Which pixels belong to the person.
pub fn scene_truth(w: usize, h: usize) -> Vec<bool> {
    (0..w * h).map(|n| at(w, h, n % w, n / w).1).collect()
}

/// The scene as tightly packed BT.709 limited-range NV12 (even `w`, `h`).
pub fn scene_nv12(w: usize, h: usize) -> Vec<u8> {
    let yuv = |rgb: [u8; 3]| {
        let [r, g, b] = rgb.map(f32::from);
        let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        (
            16.0 + 219.0 * y / 255.0,
            128.0 + 224.0 * (b - y) / (1.8556 * 255.0),
            128.0 + 224.0 * (r - y) / (1.5748 * 255.0),
        )
    };
    let mut out = Vec::with_capacity(w * h * 3 / 2);
    for y in 0..h {
        for x in 0..w {
            out.push(yuv(at(w, h, x, y).0).0.round() as u8);
        }
    }
    for y in (0..h).step_by(2) {
        for x in (0..w).step_by(2) {
            let (u, v) = (yuv(at(w, h, x, y).0).1, yuv(at(w, h, x, y).0).2);
            out.push(u.round() as u8);
            out.push(v.round() as u8);
        }
    }
    out
}

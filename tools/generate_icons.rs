//! Original GPU icons, drawn mathematically without external graphics dependencies.
use std::{f64::consts::PI, fs};
type Color = [u8; 3];
const DARK: Color = [20, 31, 43];
const MINT: Color = [45, 212, 191];
const GOLD: Color = [250, 196, 89];
const LIGHT: Color = [224, 249, 246];

fn rounded(x: f64, y: f64, left: f64, top: f64, width: f64, height: f64, radius: f64) -> bool {
    let dx = (x - left - width / 2.0).abs() - (width / 2.0 - radius);
    let dy = (y - top - height / 2.0).abs() - (height / 2.0 - radius);
    dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) <= radius
}
fn polygon(x: f64, y: f64, points: &[(f64, f64)]) -> bool {
    let mut inside = false;
    let mut previous = points.len() - 1;
    for current in 0..points.len() {
        let (a, b) = points[current];
        let (c, d) = points[previous];
        if (b > y) != (d > y) && x < (c - a) * (y - b) / (d - b) + a {
            inside = !inside;
        }
        previous = current;
    }
    inside
}
fn pixel(x: f64, y: f64, warning: bool) -> Option<Color> {
    let accent = if warning { GOLD } else { MINT };
    let mut color = None;
    for contact in 0..6 {
        if rounded(x, y, 7.0 + contact as f64 * 3.0, 24.0, 1.8, 4.4, 0.4) {
            color = Some(GOLD);
        }
    }
    if rounded(x, y, 2.0, 5.5, 28.0, 20.0, 3.0) {
        color = Some(accent);
    }
    if rounded(x, y, 3.7, 7.2, 24.6, 16.6, 1.8) {
        color = Some(DARK);
    }
    let (dx, dy) = (x - 12.5, y - 15.5);
    let radius = dx.hypot(dy);
    if (5.65..=6.5).contains(&radius) {
        color = Some(accent);
    }
    for blade in 0..6 {
        let angle = blade as f64 * PI / 3.0;
        let u = dx * angle.cos() + dy * angle.sin();
        let v = -dx * angle.sin() + dy * angle.cos();
        if ((u - 3.1) / 2.1).powi(2) + ((v - 0.65) / 0.8).powi(2) <= 1.0 {
            color = Some(LIGHT);
        }
    }
    if radius <= 1.25 {
        color = Some(accent);
    }
    if warning {
        if polygon(x, y, &[(24.0, 9.2), (19.2, 20.7), (28.5, 20.7)]) {
            color = Some(GOLD);
        }
        if rounded(x, y, 23.2, 12.3, 1.6, 4.8, 0.4) || (x - 24.0).hypot(y - 18.5) < 0.85 {
            color = Some(DARK);
        }
    } else if polygon(
        x,
        y,
        &[
            (23.1, 9.3),
            (19.7, 16.4),
            (22.7, 16.4),
            (21.4, 21.5),
            (27.0, 13.9),
            (23.8, 13.9),
        ],
    ) {
        color = Some(LIGHT);
    }
    color
}
fn bitmap(size: u32, warning: bool) -> Vec<u8> {
    let mask_stride = size.div_ceil(32) * 4;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&40u32.to_le_bytes());
    bytes.extend_from_slice(&(size as i32).to_le_bytes());
    bytes.extend_from_slice(&((size * 2) as i32).to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&32u16.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&(size * size * 4 + mask_stride * size).to_le_bytes());
    bytes.extend_from_slice(&[0; 16]);
    let mut masks = vec![0; (mask_stride * size) as usize];
    for row in 0..size {
        for column in 0..size {
            let mut sum = [0u32; 3];
            let mut coverage = 0u32;
            for sy in 0..4 {
                for sx in 0..4 {
                    let x = (column as f64 + (sx as f64 + 0.5) / 4.0) * 32.0 / size as f64;
                    let y =
                        ((size - 1 - row) as f64 + (sy as f64 + 0.5) / 4.0) * 32.0 / size as f64;
                    if let Some(color) = pixel(x, y, warning) {
                        coverage += 1;
                        for channel in 0..3 {
                            sum[channel] += color[channel] as u32;
                        }
                    }
                }
            }
            if coverage == 0 {
                bytes.extend_from_slice(&[0; 4]);
                masks[(row * mask_stride + column / 8) as usize] |= 0x80 >> (column % 8);
            } else {
                bytes.extend_from_slice(&[
                    (sum[2] / coverage) as u8,
                    (sum[1] / coverage) as u8,
                    (sum[0] / coverage) as u8,
                    ((coverage * 255 + 8) / 16) as u8,
                ]);
            }
        }
    }
    bytes.extend_from_slice(&masks);
    bytes
}
fn icon(warning: bool) -> Vec<u8> {
    let sizes = [16, 20, 24, 32, 40, 48, 64, 256];
    let images: Vec<_> = sizes
        .into_iter()
        .map(|size| bitmap(size, warning))
        .collect();
    let mut file = vec![0, 0, 1, 0, sizes.len() as u8, 0];
    let mut offset = 6 + sizes.len() * 16;
    for (size, image) in sizes.iter().zip(&images) {
        file.extend_from_slice(&[*size as u8, *size as u8, 0, 0]);
        file.extend_from_slice(&1u16.to_le_bytes());
        file.extend_from_slice(&32u16.to_le_bytes());
        file.extend_from_slice(&(image.len() as u32).to_le_bytes());
        file.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += image.len();
    }
    for image in images {
        file.extend_from_slice(&image);
    }
    file
}
fn main() {
    fs::create_dir_all("assets/icons").unwrap();
    fs::write("assets/icons/monitor.ico", icon(false)).unwrap();
    fs::write("assets/icons/warning.ico", icon(true)).unwrap();
}

// Tray icons are drawn here rather than shipped as binary assets, so the icon can
// encode status by colour on both platforms. Raw RGBA — Tauri takes it directly.

use tauri::image::Image;

// A ring — reads as a port or tunnel mouth, and stays legible at 16px.
// Supersampled 3x3 per pixel so the curve is not jagged.
pub fn ring(size: u32, [r, g, b]: [u8; 3]) -> Vec<u8> {
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    let c = (size as f64 - 1.0) / 2.0;
    let outer = size as f64 * 0.46;
    let inner = size as f64 * 0.2;
    const S: u32 = 3;

    for y in 0..size {
        for x in 0..size {
            let mut hits = 0;
            for sy in 0..S {
                for sx in 0..S {
                    let px = x as f64 + (sx as f64 + 0.5) / S as f64 - 0.5;
                    let py = y as f64 + (sy as f64 + 0.5) / S as f64 - 0.5;
                    let d = (px - c).hypot(py - c);
                    if d <= outer && d >= inner {
                        hits += 1;
                    }
                }
            }
            let i = ((y * size + x) * 4) as usize;
            rgba[i] = r;
            rgba[i + 1] = g;
            rgba[i + 2] = b;
            rgba[i + 3] = ((hits as f64 / (S * S) as f64) * 255.0).round() as u8;
        }
    }
    rgba
}

pub fn color(status: &str) -> [u8; 3] {
    match status {
        "ready" => [91, 157, 255],   // blue  — credentials loaded, no tunnels
        "active" => [63, 191, 127],  // green — tunnels open
        "warn" => [232, 179, 57],    // amber — starting, or expiring soon
        "error" => [240, 102, 95],   // red   — a tunnel failed or credentials expired
        _ => [143, 150, 171],        // grey  — no credentials
    }
}

// 32px covers Retina and Windows at 200%; the OS scales it down for 1x.
pub fn icon(status: &str) -> Image<'static> {
    const SIZE: u32 = 32;
    Image::new_owned(ring(SIZE, color(status)), SIZE, SIZE)
}

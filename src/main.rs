#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
#[cfg(feature = "autotest")]
mod autotest;
mod diff;
mod dircmp;
mod highlight;
mod settings;
mod text;
mod theme;
mod ui;

use std::path::PathBuf;

fn main() -> eframe::Result {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("DirMerge")
            .with_app_id("dir-merge")
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([760.0, 480.0])
            .with_drag_and_drop(true)
            .with_icon(app_icon()),
        ..Default::default()
    };
    eframe::run_native("DirMerge", options, Box::new(move |cc| Ok(Box::new(app::DirMergeApp::new(cc, args)))))
}

/// A small procedurally drawn icon: two overlapping rounded panes.
fn app_icon() -> egui::IconData {
    const N: usize = 128;
    let mut rgba = vec![0u8; N * N * 4];
    let rounded = |x: f32, y: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32| -> bool {
        let cx = x.clamp(x0 + r, x1 - r);
        let cy = y.clamp(y0 + r, y1 - r);
        (x - cx).powi(2) + (y - cy).powi(2) <= r * r && x >= x0 && x <= x1 && y >= y0 && y <= y1
    };
    for y in 0..N {
        for x in 0..N {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let mut px = [0u8; 4];
            if rounded(fx, fy, 8.0, 8.0, 120.0, 120.0, 26.0) {
                px = [30, 34, 42, 255];
                if rounded(fx, fy, 22.0, 26.0, 62.0, 102.0, 6.0) {
                    px = [199, 146, 234, 255];
                }
                if rounded(fx, fy, 66.0, 26.0, 106.0, 102.0, 6.0) {
                    px = [86, 194, 184, 255];
                }
                // "Diff lines".
                let line = |yy: f32| (fy - yy).abs() < 3.0;
                if (fx > 28.0 && fx < 56.0 && (line(44.0) || line(64.0))) || (fx > 72.0 && fx < 100.0 && (line(44.0) || line(84.0))) {
                    px = [30, 34, 42, 255];
                }
            }
            rgba[(y * N + x) * 4..(y * N + x) * 4 + 4].copy_from_slice(&px);
        }
    }
    egui::IconData { rgba, width: N as u32, height: N as u32 }
}

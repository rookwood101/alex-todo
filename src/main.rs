#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod board;
mod camera;
mod canvas;
#[cfg(test)]
mod headless_tests;
mod persist;
#[allow(dead_code)] // wired into the app by tasks 040/170
mod store;
mod theme;

use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Rustboard")
            .with_app_id("rustboard")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([720.0, 480.0]),
        renderer: eframe::Renderer::Wgpu,
        wgpu_options: eframe::WgpuConfiguration {
            surface: eframe::SurfaceConfig {
                present_mode: eframe::wgpu::PresentMode::AutoNoVsync,
                desired_maximum_frame_latency: Some(1),
            },
            ..Default::default()
        },
        centered: true,
        persist_window: true,
        ..Default::default()
    };

    eframe::run_native(
        "Rustboard",
        options,
        Box::new(|cc| Ok(Box::new(app::RustboardApp::new(cc)))),
    )
}

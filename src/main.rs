#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// Module homes are scaffolded up front so parallel task agents never need to
// edit this file. Everything the v2 build adds is wired in by task 170.
#[allow(dead_code)] // wired by tasks 120/130
mod agent;
mod app;
mod board;
mod camera;
mod canvas;
#[allow(dead_code)] // wired by tasks 080/090
mod capture;
#[allow(dead_code)] // wired by tasks 040/050
mod command;
#[allow(dead_code)] // wired by task 170
mod config;
#[cfg(test)]
mod headless_tests;
#[allow(dead_code)] // wired by tasks 050/130
mod history;
#[allow(dead_code)] // wired by tasks 100/170
mod layout;
#[allow(dead_code)] // wired by task 110
mod mcp;
#[allow(dead_code)] // wired by task 170
mod model;
#[allow(dead_code)] // wired by tasks 150/170
mod notify;
mod persist;
#[allow(dead_code)] // wired by tasks 160/170
mod platform;
#[allow(dead_code)] // wired by tasks 140/170
mod remind;
#[allow(dead_code)] // wired into the app by tasks 040/170
mod store;
mod theme;
#[allow(dead_code)] // wired by tasks 040/060
mod time;
#[allow(dead_code)] // wired by tasks 090/130
mod ui;

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

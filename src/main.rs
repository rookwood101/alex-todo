#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::OnceLock;

use eframe::egui;
use rustboard::app::{RustboardApp, start_platform};

/// The egui context is created by eframe, after the platform thread starts.
/// The hotkey/tray thread uses this to wake the UI once it exists.
static UI_CONTEXT: OnceLock<egui::Context> = OnceLock::new();

fn main() -> eframe::Result {
    let (platform, forwarded_to_existing) = start_platform(|| {
        if let Some(ctx) = UI_CONTEXT.get() {
            ctx.request_repaint();
        }
    });
    if forwarded_to_existing {
        // A second launch asked the running instance to show itself.
        return Ok(());
    }

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
        Box::new(move |cc| {
            let _ = UI_CONTEXT.set(cc.egui_ctx.clone());
            Ok(Box::new(RustboardApp::new(cc, platform)))
        }),
    )
}

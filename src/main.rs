// Release builds on Windows have no console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::Renderer;
use multirecorder::app::MultiRecorderApp;

fn run(renderer: Renderer) -> eframe::Result {
    let options = eframe::NativeOptions {
        renderer,
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("MultiRecorder")
            .with_inner_size([960.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "MultiRecorder",
        options,
        Box::new(|cc| Ok(Box::new(MultiRecorderApp::new(cc)))),
    )
}

fn main() -> eframe::Result {
    env_logger::init();
    // wgpu (DX12/Vulkan/Metal) is preferred; OpenGL covers machines where it fails to start.
    run(Renderer::Wgpu).or_else(|e| {
        log::warn!("wgpu renderer failed ({e}); falling back to OpenGL");
        run(Renderer::Glow)
    })
}

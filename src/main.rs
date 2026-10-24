// Release builds on Windows have no console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use multirecorder::app::MultiRecorderApp;

fn main() -> eframe::Result {
    env_logger::init();
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("MultiRecorder")
            .with_inner_size([760.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "MultiRecorder",
        options,
        Box::new(|cc| Ok(Box::new(MultiRecorderApp::new(cc)))),
    )
}

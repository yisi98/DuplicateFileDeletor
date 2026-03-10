mod gui;

fn main() -> eframe::Result<()> {
    env_logger::init();

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("DuplicateFileDeletor")
            .with_inner_size([1440.0, 920.0])
            .with_min_inner_size([1180.0, 760.0]),
        ..Default::default()
    };

    eframe::run_native(
        "DuplicateFileDeletor",
        options,
        Box::new(|cc| Ok(Box::new(gui::DedupeApp::new(cc)))),
    )
}

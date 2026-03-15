#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod gui;

fn main() -> eframe::Result<()> {
    env_logger::init();

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("DuplicateFileDeletor")
            .with_inner_size([1440.0, 920.0])
            .with_min_inner_size([1180.0, 760.0])
            .with_icon(load_app_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "DuplicateFileDeletor",
        options,
        Box::new(|cc| Ok(Box::new(gui::DedupeApp::new(cc)))),
    )
}

fn load_app_icon() -> eframe::egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/app-icon.png"))
        .expect("embedded app icon should be a valid PNG")
}

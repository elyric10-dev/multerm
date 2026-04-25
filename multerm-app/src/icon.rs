const DEFAULT_APP_ICON_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/icons/multerm_logo_no_bg.png"
);

fn app_icon_path() -> String {
    std::env::var("MULTERM_APP_ICON").unwrap_or_else(|_| DEFAULT_APP_ICON_PATH.to_owned())
}

fn load_icon_rgba() -> Option<(Vec<u8>, u32, u32)> {
    let path = app_icon_path();
    let img = match image::open(&path) {
        Ok(img) => img,
        Err(err) => {
            tracing::warn!("Could not load app icon from {}: {}", path, err);
            return None;
        }
    };
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    Some((rgba.into_raw(), width, height))
}

#[allow(dead_code)]
pub fn load_egui_icon_data() -> Option<eframe::egui::IconData> {
    let (rgba, width, height) = load_icon_rgba()?;
    Some(eframe::egui::IconData {
        rgba,
        width,
        height,
    })
}

#[allow(dead_code)]
pub fn load_winit_window_icon() -> Option<winit::window::Icon> {
    let (rgba, width, height) = load_icon_rgba()?;
    match winit::window::Icon::from_rgba(rgba, width, height) {
        Ok(icon) => Some(icon),
        Err(err) => {
            tracing::warn!("Could not construct window icon from app image: {}", err);
            None
        }
    }
}

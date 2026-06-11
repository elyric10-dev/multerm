use std::path::PathBuf;

const DEFAULT_APP_ICON: &[u8] = include_bytes!("../assets/icons/multerm_logo_no_bg.png");

fn bundled_app_icon_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // macOS .app: Contents/MacOS/<bin> → Contents/Resources/AppIcon.png
    let resources = exe
        .parent()?
        .parent()?
        .join("Resources")
        .join("AppIcon.png");
    if resources.is_file() {
        return Some(resources);
    }
    // Generic: icon next to the executable (Linux tarball layout).
    let sibling = exe
        .parent()?
        .join("AppIcon.png");
    if sibling.is_file() {
        return Some(sibling);
    }
    None
}

fn load_icon_rgba() -> Option<(Vec<u8>, u32, u32)> {
    if let Ok(path) = std::env::var("MULTERM_APP_ICON") {
        if let Ok(img) = image::open(&path) {
            let rgba = img.to_rgba8();
            let (width, height) = rgba.dimensions();
            return Some((rgba.into_raw(), width, height));
        }
        tracing::warn!("Could not load app icon from MULTERM_APP_ICON={path}");
    }

    if let Some(path) = bundled_app_icon_path() {
        if let Ok(img) = image::open(&path) {
            let rgba = img.to_rgba8();
            let (width, height) = rgba.dimensions();
            return Some((rgba.into_raw(), width, height));
        }
        tracing::warn!("Could not load bundled app icon from {}", path.display());
    }

    let img = image::load_from_memory(DEFAULT_APP_ICON).ok()?;
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

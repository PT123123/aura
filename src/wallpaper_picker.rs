//! Slint-based wallpaper picker window (Windows only).
//!
//! Lists every wallpaper currently available to aura (downloaded remote
//! cache images plus any local file/directory sources) as a thumbnail grid.
//! Clicking "应用" applies that image to every display via the main loop.

use crate::config::SourceConfig;
use crate::errors::Result;
use crate::tray::TrayEvent;
use anyhow::Context;
use slint::{ComponentHandle, Image as SlintImage, ModelRc, SharedString, VecModel};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

static PICKER_WINDOW_OPEN: AtomicBool = AtomicBool::new(false);

const THUMB_WIDTH: u32 = 320;

pub fn open_wallpaper_picker(
    remote_images: Vec<PathBuf>,
    local_images: Vec<PathBuf>,
    tray_event_tx: UnboundedSender<TrayEvent>,
) {
    if PICKER_WINDOW_OPEN.swap(true, Ordering::SeqCst) {
        info!("wallpaper picker is already open; ignoring duplicate request");
        return;
    }
    thread::spawn(move || {
        let result = run_picker(remote_images, local_images, &tray_event_tx);
        PICKER_WINDOW_OPEN.store(false, Ordering::SeqCst);
        if let Err(error) = result {
            warn!(error = %error, "wallpaper picker window failed");
        }
    });
}

/// Collect image files from local `File`/`Directory` sources.
pub fn collect_local_images(sources: &[SourceConfig]) -> Vec<PathBuf> {
    let mut images = Vec::new();
    for source in sources {
        match source {
            SourceConfig::File { path } => {
                if is_supported_image(path) {
                    images.push(path.clone());
                }
            }
            SourceConfig::Directory { path, recursive, .. } => {
                let walker = walkdir::WalkDir::new(path);
                let walker = if *recursive {
                    walker
                } else {
                    walker.max_depth(1)
                };
                for entry in walker.into_iter().flatten() {
                    let entry_path = entry.path();
                    if entry_path.is_file() && is_supported_image(entry_path) {
                        images.push(entry_path.to_path_buf());
                    }
                }
            }
            SourceConfig::Rss { .. } | SourceConfig::Wallhaven { .. } => {}
        }
    }
    images
}

fn run_picker(
    remote_images: Vec<PathBuf>,
    local_images: Vec<PathBuf>,
    tray_event_tx: &UnboundedSender<TrayEvent>,
) -> Result<()> {
    let ui = crate::settings::WallpaperPickerWindow::new()
        .context("failed to create wallpaper picker window")?;

    let mut entries = Vec::new();
    for path in remote_images.into_iter().chain(local_images) {
        if let Some(thumb) = make_thumbnail(&path) {
            let name = path
                .file_name()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_default();
            entries.push(crate::settings::WallpaperEntry {
                path: SharedString::from(path.to_string_lossy().into_owned()),
                thumb,
                name: SharedString::from(name),
            });
        }
    }
    info!(count = entries.len(), "wallpaper picker listed images");
    if entries.is_empty() {
        return Err(anyhow::anyhow!(
            "no wallpapers available yet; wait for the first remote download"
        ));
    }
    let model: ModelRc<crate::settings::WallpaperEntry> =
        std::rc::Rc::new(VecModel::from(entries)).into();
    ui.set_entries(model);

    let tx = tray_event_tx.clone();
    let weak = ui.as_weak();
    ui.on_choose(move |path| {
        let _ = tx.send(TrayEvent::ApplyWallpaper(PathBuf::from(path.as_str())));
        if let Some(ui) = weak.upgrade() {
            let _ = ui.hide();
        }
    });

    let weak = ui.as_weak();
    ui.on_request_close(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = ui.hide();
        }
    });

    ui.run().context("wallpaper picker event loop failed")?;
    Ok(())
}

fn make_thumbnail(path: &Path) -> Option<SlintImage> {
    let source = image::open(path).ok()?;
    let (width, height) = image::GenericImageView::dimensions(&source);
    let scale = THUMB_WIDTH as f32 / width as f32;
    let target_height = (height as f32 * scale).max(1.0) as u32;
    let thumbnail = source.thumbnail(THUMB_WIDTH, target_height);

    let dir = std::env::temp_dir().join("aura-wallpaper-thumbs");
    std::fs::create_dir_all(&dir).ok()?;
    let key = blake3::hash(path.to_string_lossy().as_bytes()).to_hex().to_string();
    let output = dir.join(format!("{key}.png"));
    thumbnail.save(&output).ok()?;
    SlintImage::load_from_path(&output).ok()
}

fn is_supported_image(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|value| value.to_str()),
        Some(ext) if matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png" | "webp" | "bmp" | "gif")
    )
}


#[cfg(test)]
mod tests {
    use super::*;
    use slint::ComponentHandle;

    /// The picker component must instantiate and accept a thumbnail model.
    /// Kept separate from other Slint-window tests (winit initializes once).
    #[test]
    fn picker_window_instantiates_and_accepts_entries() {
        let ui = crate::settings::WallpaperPickerWindow::new().unwrap();
        let entries = vec![crate::settings::WallpaperEntry {
            path: SharedString::from("C:\\fake\\wallpaper.jpg"),
            thumb: SlintImage::default(),
            name: SharedString::from("wallpaper.jpg"),
        }];
        let model: ModelRc<crate::settings::WallpaperEntry> =
            std::rc::Rc::new(VecModel::from(entries)).into();
        ui.set_entries(model);
        ui.on_choose(|_| {});
        ui.on_request_close(|| {});
        let _ = ui.hide();
    }

    /// Thumbnail generation must work for a real image.
    #[test]
    fn thumbnail_generation_round_trips() {
        let dir = std::env::temp_dir().join("aura-picker-test");
        std::fs::create_dir_all(&dir).unwrap();
        let source = image::RgbaImage::from_pixel(800, 600, image::Rgba([10, 120, 200, 255]));
        let source_path = dir.join("source.png");
        source.save(&source_path).unwrap();

        let thumb = make_thumbnail(&source_path);
        assert!(thumb.is_some(), "thumbnail should load from disk");
        std::fs::remove_dir_all(&dir).ok();
    }
}

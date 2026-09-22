//! Slint-based wallpaper picker window (Windows only).
//!
//! Lists every wallpaper currently available to aura (downloaded remote
//! cache images plus any local file/directory sources) as a thumbnail grid.
//! Right-clicking a thumbnail opens a context menu to apply that image to a
//! specific monitor or to toggle it in the favorites list.

use crate::config::SourceConfig;
use crate::errors::Result;
use crate::tray::TrayEvent;
use anyhow::Context;
use slint::{ComponentHandle, Image as SlintImage, ModelRc, SharedString, VecModel};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

static PICKER_WINDOW_OPEN: AtomicBool = AtomicBool::new(false);

const THUMB_WIDTH: u32 = 320;
const GRID_COLUMNS: usize = 4;

/// (wallpaper path, display name, thumbnail file path). Pure `String` so the
/// collection is `Send`; `slint::Image` is not `Send` and is loaded on the
/// UI thread inside `refresh_cells`.
type LoadedThumb = (String, String, String);

pub fn open_wallpaper_picker(
    remote_images: Vec<PathBuf>,
    local_images: Vec<PathBuf>,
    monitor_names: Vec<String>,
    favorites: Vec<String>,
    tray_event_tx: UnboundedSender<TrayEvent>,
) {
    if PICKER_WINDOW_OPEN.swap(true, Ordering::SeqCst) {
        info!("wallpaper picker is already open; ignoring duplicate request");
        return;
    }
    thread::spawn(move || {
        let result = run_picker(
            remote_images,
            local_images,
            monitor_names,
            favorites,
            &tray_event_tx,
        );
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
    monitor_names: Vec<String>,
    favorites: Vec<String>,
    tray_event_tx: &UnboundedSender<TrayEvent>,
) -> Result<()> {
    let ui = crate::settings::WallpaperPickerWindow::new()
        .context("failed to create wallpaper picker window")?;

    // Show the window immediately with an empty grid; thumbnails are
    // generated on a worker thread and swapped in when ready so the UI
    // never appears stuck for large caches.
    let empty_cells: ModelRc<crate::settings::WallpaperCell> =
        std::rc::Rc::new(VecModel::from(Vec::<crate::settings::WallpaperCell>::new())).into();
    ui.set_cells(empty_cells);

    let monitor_model: ModelRc<SharedString> = std::rc::Rc::new(VecModel::from(
        monitor_names
            .iter()
            .map(|name| SharedString::from(name.as_str()))
            .collect::<Vec<_>>(),
    ))
    .into();
    ui.set_monitor_names(monitor_model);

    let favorite_paths = Arc::new(Mutex::new(
        favorites.iter().cloned().collect::<HashSet<String>>(),
    ));
    let loaded_entries = Arc::new(Mutex::new(Vec::<LoadedThumb>::new()));

    let tx = tray_event_tx.clone();
    let weak = ui.as_weak();
    ui.on_apply_all(move |path| {
        let _ = tx.send(TrayEvent::ApplyWallpaper(PathBuf::from(path.as_str())));
        if let Some(ui) = weak.upgrade() {
            let _ = ui.hide();
        }
    });

    let tx = tray_event_tx.clone();
    ui.on_apply_to_monitor(move |path, monitor_index| {
        let _ = tx.send(TrayEvent::ApplyWallpaperToMonitor(
            PathBuf::from(path.as_str()),
            monitor_index as usize,
        ));
    });

    // Favorite toggle: forward to the main loop (which persists it) and
    // refresh the grid's favorite markers locally.
    let tx = tray_event_tx.clone();
    let favorite_paths_clone = favorite_paths.clone();
    let loaded_entries_cb = loaded_entries.clone();
    let ui_weak = ui.as_weak();
    ui.on_add_favorite(move |path| {
        let key = path.to_string();
        {
            let mut favs = favorite_paths_clone.lock().expect("favorite set poisoned");
            if favs.contains(&key) {
                favs.remove(&key);
            } else {
                favs.insert(key.clone());
            }
        }
        let _ = tx.send(TrayEvent::AddFavorite(PathBuf::from(key)));
        if let Some(ui) = ui_weak.upgrade() {
            refresh_cells(&ui, &loaded_entries_cb, &favorite_paths_clone);
        }
    });

    let weak = ui.as_weak();
    ui.on_close_window(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = ui.hide();
        }
    });

    let paths = remote_images
        .into_iter()
        .chain(local_images)
        .collect::<Vec<_>>();
    let weak = ui.as_weak();
    let loaded_entries_thread = loaded_entries.clone();
    thread::spawn(move || {
        let mut items = Vec::new();
        for path in &paths {
            if let Some((display_path, thumb_path)) = make_thumbnail(path) {
                items.push((display_path, thumb_path));
            }
        }
        info!(count = items.len(), "wallpaper picker prepared thumbnails");
        let loaded = loaded_entries_thread.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                let mut entries = Vec::new();
                for (display_path, thumb_path) in items {
                    let name = Path::new(&display_path)
                        .file_name()
                        .map(|value| value.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    entries.push((display_path, name, thumb_path));
                }
                if let Ok(mut guard) = loaded.lock() {
                    *guard = entries;
                }
                refresh_cells(&ui, &loaded_entries, &favorite_paths);
                ui.set_loading(false);
            }
        });
    });

    ui.run().context("wallpaper picker event loop failed")?;
    Ok(())
}

/// Rebuild the `cells` model (GRID_COLUMNS columns per row) from the loaded
/// entries, marking each entry as favorited according to `favorites`.
fn refresh_cells(
    ui: &crate::settings::WallpaperPickerWindow,
    loaded_entries: &Arc<Mutex<Vec<LoadedThumb>>>,
    favorites: &Arc<Mutex<HashSet<String>>>,
) {
    let entries = match loaded_entries.lock() {
        Ok(guard) => guard.clone(),
        Err(_) => return,
    };
    if entries.is_empty() {
        return;
    }
    let favs = favorites.lock().expect("favorite set poisoned");
    let mut cells: Vec<crate::settings::WallpaperCell> = Vec::with_capacity(entries.len());
    for (index, (path, name, thumb_path)) in entries.iter().enumerate() {
        let Some(thumb) = SlintImage::load_from_path(Path::new(thumb_path)).ok() else {
            continue;
        };
        cells.push(crate::settings::WallpaperCell {
            path: SharedString::from(path.as_str()),
            thumb,
            name: SharedString::from(name.as_str()),
            favorited: favs.contains(path.as_str()),
            row: (index / GRID_COLUMNS) as i32,
            col: (index % GRID_COLUMNS) as i32,
        });
    }
    let model: ModelRc<crate::settings::WallpaperCell> =
        std::rc::Rc::new(VecModel::from(cells)).into();
    ui.set_cells(model);
}

/// Create (or reuse) a thumbnail file; returns (display path, thumbnail path).
fn make_thumbnail(path: &Path) -> Option<(String, String)> {
    let dir = std::env::temp_dir().join("aura-wallpaper-thumbs");
    std::fs::create_dir_all(&dir).ok()?;
    let key = blake3::hash(path.to_string_lossy().as_bytes()).to_hex().to_string();
    let output = dir.join(format!("{key}.png"));
    if !output.exists() {
        let source = image::open(path).ok()?;
        let (width, height) = image::GenericImageView::dimensions(&source);
        let scale = THUMB_WIDTH as f32 / width as f32;
        let target_height = (height as f32 * scale).max(1.0) as u32;
        let thumbnail = source.thumbnail(THUMB_WIDTH, target_height);
        if let Err(error) = thumbnail.save(&output) {
            warn!(error = %error, path = %path.display(), "failed to save wallpaper thumbnail");
            return None;
        }
    }
    Some((
        path.to_string_lossy().into_owned(),
        output.to_string_lossy().into_owned(),
    ))
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
        let cell = crate::settings::WallpaperCell {
            path: SharedString::from("C:\\fake\\wallpaper.jpg"),
            thumb: SlintImage::default(),
            name: SharedString::from("wallpaper.jpg"),
            favorited: true,
            row: 0,
            col: 0,
        };
        let cells: ModelRc<crate::settings::WallpaperCell> =
            std::rc::Rc::new(VecModel::from(vec![cell])).into();
        ui.set_cells(cells);
        ui.on_apply_all(|_| {});
        ui.on_apply_to_monitor(|_, _| {});
        ui.on_add_favorite(|_| {});
        ui.on_close_window(|| {});
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

//! Slint-based wallpaper picker window (Windows only).
//!
//! Lists every wallpaper currently available to aura (downloaded remote
//! cache images plus any local file/directory sources) as a thumbnail grid.
//! Supports live search/favorites filtering, click-to-select with an apply
//! footer, and a native right-click context menu to apply that image to a
//! specific monitor or toggle it in the favorites list.

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
/// Push a progressive grid update after this many new thumbnails.
const PROGRESS_BATCH: usize = 6;

/// (wallpaper path, display name, thumbnail file path). Pure `String` so the
/// collection is `Send`; `slint::Image` is not `Send` and is loaded on the
/// UI thread inside `refresh_cells`.
type LoadedThumb = (String, String, String);

/// Current search/favorites filter applied when rebuilding the grid.
#[derive(Default)]
struct PickerFilter {
    search: String,
    favorites_only: bool,
}

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
            SourceConfig::Directory {
                path, recursive, ..
            } => {
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
    let filter = Arc::new(Mutex::new(PickerFilter::default()));

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
    let filter_cb = filter.clone();
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
            refresh_cells(&ui, &loaded_entries_cb, &favorite_paths_clone, &filter_cb);
        }
    });

    let loaded_entries_cb = loaded_entries.clone();
    let favorite_paths_cb = favorite_paths.clone();
    let filter_cb = filter.clone();
    let ui_weak = ui.as_weak();
    ui.on_filter_changed(move |search, favorites_only| {
        if let Ok(mut guard) = filter_cb.lock() {
            guard.search = search.to_string();
            guard.favorites_only = favorites_only;
        }
        if let Some(ui) = ui_weak.upgrade() {
            refresh_cells(&ui, &loaded_entries_cb, &favorite_paths_cb, &filter_cb);
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
    let favorite_paths_thread = favorite_paths.clone();
    let filter_thread = filter.clone();
    thread::spawn(move || {
        let mut batch: Vec<LoadedThumb> = Vec::new();
        let mut total = 0usize;
        for path in &paths {
            let Some((display_path, thumb_path)) = make_thumbnail(path) else {
                continue;
            };
            let name = Path::new(&display_path)
                .file_name()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_default();
            batch.push((display_path, name, thumb_path));
            total += 1;
            if batch.len() >= PROGRESS_BATCH {
                let ready = std::mem::take(&mut batch);
                push_progress(
                    &weak,
                    &loaded_entries_thread,
                    &favorite_paths_thread,
                    &filter_thread,
                    ready,
                    false,
                );
            }
        }
        info!(count = total, "wallpaper picker prepared thumbnails");
        push_progress(
            &weak,
            &loaded_entries_thread,
            &favorite_paths_thread,
            &filter_thread,
            batch,
            true,
        );
    });

    ui.run().context("wallpaper picker event loop failed")?;
    Ok(())
}

/// Merge a batch of thumbnails into the shared list and refresh the grid on
/// the UI thread; `done` also clears the loading indicator.
fn push_progress(
    ui_weak: &slint::Weak<crate::settings::WallpaperPickerWindow>,
    loaded_entries: &Arc<Mutex<Vec<LoadedThumb>>>,
    favorites: &Arc<Mutex<HashSet<String>>>,
    filter: &Arc<Mutex<PickerFilter>>,
    batch: Vec<LoadedThumb>,
    done: bool,
) {
    let ui_weak = ui_weak.clone();
    let loaded_entries = loaded_entries.clone();
    let favorites = favorites.clone();
    let filter = filter.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = ui_weak.upgrade() else {
            return;
        };
        if !batch.is_empty() {
            if let Ok(mut guard) = loaded_entries.lock() {
                guard.extend(batch);
            }
        }
        refresh_cells(&ui, &loaded_entries, &favorites, &filter);
        if done {
            ui.set_loading(false);
        }
    });
}

/// Rebuild the `cells` model from the loaded entries, applying the current
/// search/favorites filter and marking each entry as favorited.
fn refresh_cells(
    ui: &crate::settings::WallpaperPickerWindow,
    loaded_entries: &Arc<Mutex<Vec<LoadedThumb>>>,
    favorites: &Arc<Mutex<HashSet<String>>>,
    filter: &Arc<Mutex<PickerFilter>>,
) {
    let entries = match loaded_entries.lock() {
        Ok(guard) => guard.clone(),
        Err(_) => return,
    };
    let favs = match favorites.lock() {
        Ok(guard) => guard.clone(),
        Err(_) => return,
    };
    let (search, favorites_only) = match filter.lock() {
        Ok(guard) => (guard.search.to_lowercase(), guard.favorites_only),
        Err(_) => (String::new(), false),
    };

    let total = entries.len() as i32;
    let mut cells: Vec<crate::settings::WallpaperCell> = Vec::with_capacity(entries.len());
    for (path, name, thumb_path) in &entries {
        if favorites_only && !favs.contains(path.as_str()) {
            continue;
        }
        if !search.is_empty()
            && !name.to_lowercase().contains(&search)
            && !path.to_lowercase().contains(&search)
        {
            continue;
        }
        let Some(thumb) = SlintImage::load_from_path(Path::new(thumb_path)).ok() else {
            continue;
        };
        cells.push(crate::settings::WallpaperCell {
            path: SharedString::from(path.as_str()),
            thumb,
            name: SharedString::from(name.as_str()),
            favorited: favs.contains(path.as_str()),
        });
    }
    ui.set_total_count(total);
    ui.set_shown_count(cells.len() as i32);
    let model: ModelRc<crate::settings::WallpaperCell> =
        std::rc::Rc::new(VecModel::from(cells)).into();
    ui.set_cells(model);
}

/// Create (or reuse) a thumbnail file; returns (display path, thumbnail path).
fn make_thumbnail(path: &Path) -> Option<(String, String)> {
    let dir = std::env::temp_dir().join("aura-wallpaper-thumbs");
    std::fs::create_dir_all(&dir).ok()?;
    let key = blake3::hash(path.to_string_lossy().as_bytes())
        .to_hex()
        .to_string();
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

    /// The picker component must instantiate, accept a thumbnail model, and
    /// apply search/favorites filtering. Runs on the shared Slint test thread
    /// because winit only allows a single platform/event loop per process.
    #[test]
    fn picker_window_filters_entries() {
        crate::settings::run_slint_test(|| {
            use slint::Model;

            let ui = crate::settings::WallpaperPickerWindow::new().unwrap();
            let cell = crate::settings::WallpaperCell {
                path: SharedString::from("C:\\fake\\wallpaper.jpg"),
                thumb: SlintImage::default(),
                name: SharedString::from("wallpaper.jpg"),
                favorited: true,
            };
            let cells: ModelRc<crate::settings::WallpaperCell> =
                std::rc::Rc::new(VecModel::from(vec![cell])).into();
            ui.set_cells(cells);
            ui.on_apply_all(|_| {});
            ui.on_apply_to_monitor(|_, _| {});
            ui.on_add_favorite(|_| {});
            ui.on_filter_changed(|_, _| {});
            ui.on_close_window(|| {});

            let dir = std::env::temp_dir().join("aura-picker-filter-test");
            std::fs::create_dir_all(&dir).unwrap();
            let make_thumb = |name: &str| -> LoadedThumb {
                let path = dir.join(name);
                image::RgbaImage::from_pixel(8, 8, image::Rgba([20, 40, 60, 255]))
                    .save(&path)
                    .unwrap();
                let wallpaper = dir.join(format!("wallpaper-{name}"));
                (
                    wallpaper.to_string_lossy().into_owned(),
                    name.into(),
                    path.to_string_lossy().into_owned(),
                )
            };
            let entries: Arc<Mutex<Vec<LoadedThumb>>> = Arc::new(Mutex::new(vec![
                make_thumb("aurora.png"),
                make_thumb("beach.png"),
            ]));
            let beach_path = entries.lock().unwrap()[1].0.clone();
            let favorites: Arc<Mutex<HashSet<String>>> =
                Arc::new(Mutex::new(HashSet::from([beach_path])));
            let filter = Arc::new(Mutex::new(PickerFilter::default()));

            filter.lock().unwrap().search = "aurora".into();
            refresh_cells(&ui, &entries, &favorites, &filter);
            assert_eq!(ui.get_shown_count(), 1);
            assert_eq!(ui.get_total_count(), 2);

            filter.lock().unwrap().search.clear();
            filter.lock().unwrap().favorites_only = true;
            refresh_cells(&ui, &entries, &favorites, &filter);
            assert_eq!(ui.get_shown_count(), 1);
            assert_eq!(ui.get_cells().row_count(), 1);
            assert!(ui.get_cells().row_data(0).unwrap().favorited);

            filter.lock().unwrap().favorites_only = false;
            filter.lock().unwrap().search = "no-match".into();
            refresh_cells(&ui, &entries, &favorites, &filter);
            assert_eq!(ui.get_shown_count(), 0);
            assert_eq!(ui.get_cells().row_count(), 0);

            let _ = ui.hide();
            std::fs::remove_dir_all(&dir).ok();
        });
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

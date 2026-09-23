//! Slint-based wallpaper picker window (Windows only).
//!
//! Lists every wallpaper currently available to aura (downloaded remote
//! cache images plus any local file/directory sources) as a thumbnail grid.
//!
//! On top of the grid the window offers:
//! - search plus source / minimum-resolution / sort filters and a
//!   favorites-only toggle,
//! - an always-visible display chooser so an image can be applied to any
//!   combination of monitors (or to all of them at once),
//! - an inline Wallhaven fetch panel that downloads new images straight into
//!   the remote cache shared with the rotation sources and re-scans the grid
//!   when the download finishes.

use crate::cache::CacheManager;
use crate::config::SourceConfig;
use crate::errors::Result;
use crate::settings::{MonitorChip, WallpaperCell, WallpaperPickerWindow};
use crate::sources::wallhaven::WallhavenSource;
use crate::sources::ImageSource;
use crate::tray::TrayEvent;
use anyhow::Context;
use slint::{
    ComponentHandle, Image as SlintImage, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString,
    VecModel,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::UNIX_EPOCH;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

static PICKER_WINDOW_OPEN: AtomicBool = AtomicBool::new(false);

const THUMB_WIDTH: u32 = 320;
/// Push a progressive grid update after this many newly probed images.
const PROGRESS_BATCH: usize = 6;

thread_local! {
    /// Decoded thumbnails keyed by thumbnail file path. `slint::Image` is not
    /// `Send`, so the cache lives on the thread that owns the window and keeps
    /// every filter change from re-decoding the whole grid.
    static THUMB_CACHE: RefCell<HashMap<String, SlintImage>> = RefCell::new(HashMap::new());
}

/// One probeable wallpaper: everything the UI needs to render and filter it.
#[derive(Debug, Clone)]
struct PickerEntry {
    path: String,
    name: String,
    thumb_path: String,
    /// `true` when the image comes from a local file/directory source instead
    /// of the downloaded remote cache.
    local: bool,
    width: u32,
    height: u32,
    bytes: u64,
    modified: u64,
}

impl PickerEntry {
    /// Longest edge, used by the minimum-resolution filter so that portrait
    /// wallpapers are not filtered out by their short edge.
    fn longest_edge(&self) -> u32 {
        self.width.max(self.height)
    }

    fn meta_text(&self) -> String {
        format!(
            "{}×{} · {}",
            self.width,
            self.height,
            format_bytes(self.bytes)
        )
    }
}

/// Current search / source / resolution / sort filter applied when rebuilding
/// the grid.
struct PickerFilter {
    search: String,
    favorites_only: bool,
    /// 0 = every source, 1 = remote cache only, 2 = local files only.
    source_index: i32,
    /// 0 = no minimum, otherwise the required longest edge in pixels.
    min_height: u32,
    /// 0 name A→Z, 1 name Z→A, 2 newest, 3 oldest, 4 resolution, 5 file size.
    sort_index: i32,
}

impl Default for PickerFilter {
    fn default() -> Self {
        Self {
            search: String::new(),
            favorites_only: false,
            source_index: 0,
            min_height: 0,
            // Newest first so freshly downloaded wallpapers show up on top.
            sort_index: 2,
        }
    }
}

struct PickerState {
    entries: Vec<PickerEntry>,
    favorites: HashSet<String>,
    filter: PickerFilter,
    monitor_names: Vec<String>,
    monitors: Vec<bool>,
}

/// Everything the picker needs from the running app.
pub struct PickerContext {
    pub cache: Arc<CacheManager>,
    pub local_sources: Vec<SourceConfig>,
    pub monitor_names: Vec<String>,
    pub favorites: Vec<String>,
    /// Wallhaven defaults (API key, top range) taken from the config file so
    /// the fetch panel starts from the user's own source settings.
    pub default_wallhaven: Option<SourceConfig>,
    pub tray_event_tx: UnboundedSender<TrayEvent>,
}

/// One in-flight "fetch more from Wallhaven" request built from the panel.
struct FetchRequest {
    query: Option<String>,
    categories: String,
    purity: String,
    sorting: String,
    top_range: String,
    atleast: Option<String>,
    max_items: usize,
    api_key: Option<String>,
}

pub fn open_wallpaper_picker(context: PickerContext) {
    if PICKER_WINDOW_OPEN.swap(true, Ordering::SeqCst) {
        info!("wallpaper picker is already open; ignoring duplicate request");
        return;
    }
    thread::spawn(move || {
        let result = run_picker(context);
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

fn run_picker(context: PickerContext) -> Result<()> {
    let PickerContext {
        cache,
        local_sources,
        monitor_names,
        favorites,
        default_wallhaven,
        tray_event_tx,
    } = context;

    let ui = WallpaperPickerWindow::new().context("failed to create wallpaper picker window")?;

    // Show the window immediately with an empty grid; thumbnails are probed on
    // a worker thread and swapped in when ready so the UI never appears stuck
    // while a large cache is scanned.
    let empty_cells: ModelRc<WallpaperCell> =
        std::rc::Rc::new(VecModel::from(Vec::<WallpaperCell>::new())).into();
    ui.set_cells(empty_cells);

    let state = Arc::new(Mutex::new(PickerState {
        entries: Vec::new(),
        favorites: favorites.iter().cloned().collect(),
        filter: PickerFilter::default(),
        monitors: vec![false; monitor_names.len()],
        monitor_names,
    }));

    if let Ok(guard) = state.lock() {
        push_monitors(&ui, &guard);
    }

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

    // Apply to every display the user ticked in the display chooser; with no
    // tick, fall back to the whole desktop. Each monitor gets its own
    // `IDesktopWallpaper` call, so different displays can keep different
    // images by applying them one after another.
    let tx = tray_event_tx.clone();
    let state_apply = state.clone();
    let weak = ui.as_weak();
    ui.on_apply_to_selection(move |path| {
        let targets = match state_apply.lock() {
            Ok(guard) => guard
                .monitors
                .iter()
                .enumerate()
                .filter(|(_, selected)| **selected)
                .map(|(index, _)| index)
                .collect::<Vec<_>>(),
            Err(_) => Vec::new(),
        };

        let path = PathBuf::from(path.as_str());
        if targets.is_empty() {
            let _ = tx.send(TrayEvent::ApplyWallpaper(path));
        } else {
            for index in targets {
                let _ = tx.send(TrayEvent::ApplyWallpaperToMonitor(path.clone(), index));
            }
        }
        if let Some(ui) = weak.upgrade() {
            let _ = ui.hide();
        }
    });

    // Favorite toggle: forward to the main loop (which persists it) and
    // refresh the grid's favorite markers locally.
    let tx = tray_event_tx.clone();
    let state_favorite = state.clone();
    let weak = ui.as_weak();
    ui.on_add_favorite(move |path| {
        let key = path.to_string();
        {
            match state_favorite.lock() {
                Ok(mut guard) => {
                    if !guard.favorites.remove(&key) {
                        guard.favorites.insert(key.clone());
                    }
                }
                Err(_) => return,
            }
        }
        let _ = tx.send(TrayEvent::AddFavorite(PathBuf::from(key)));
        if let Some(ui) = weak.upgrade() {
            refresh_cells(&ui, &state_favorite);
        }
    });

    // Delete: drop the wallpaper from the running pool (main loop also removes
    // it from the rotation/favorites and deletes the cache file) and from this
    // window's own grid immediately.
    let tx_delete = tray_event_tx.clone();
    let state_delete = state.clone();
    let weak = ui.as_weak();
    ui.on_confirm_delete(move |path| {
        let key = path.to_string();
        if let Ok(mut guard) = state_delete.lock() {
            guard.entries.retain(|entry| entry.path != key);
            guard.favorites.remove(&key);
        }
        let _ = tx_delete.send(TrayEvent::RemoveWallpaper(PathBuf::from(key)));
        if let Some(ui) = weak.upgrade() {
            ui.set_confirm_visible(false);
            refresh_cells(&ui, &state_delete);
        }
    });

    let state_toggle = state.clone();
    let weak = ui.as_weak();
    ui.on_toggle_monitor(move |index| {
        if let Ok(mut guard) = state_toggle.lock() {
            let index = index as usize;
            if index < guard.monitors.len() {
                guard.monitors[index] = !guard.monitors[index];
            }
            if let Some(ui) = weak.upgrade() {
                push_monitors(&ui, &guard);
            }
        }
    });

    let state_select = state.clone();
    let weak = ui.as_weak();
    ui.on_select_all_monitors(move |select| {
        if let Ok(mut guard) = state_select.lock() {
            for selected in guard.monitors.iter_mut() {
                *selected = select;
            }
            if let Some(ui) = weak.upgrade() {
                push_monitors(&ui, &guard);
            }
        }
    });

    let state_search = state.clone();
    let weak = ui.as_weak();
    ui.on_search_changed(move |text| {
        if let Ok(mut guard) = state_search.lock() {
            guard.filter.search = text.to_string();
        }
        if let Some(ui) = weak.upgrade() {
            refresh_cells(&ui, &state_search);
        }
    });

    let state_fav_filter = state.clone();
    let weak = ui.as_weak();
    ui.on_favorites_toggled(move |enabled| {
        if let Ok(mut guard) = state_fav_filter.lock() {
            guard.filter.favorites_only = enabled;
        }
        if let Some(ui) = weak.upgrade() {
            refresh_cells(&ui, &state_fav_filter);
        }
    });

    let state_source = state.clone();
    let weak = ui.as_weak();
    ui.on_source_changed(move |index| {
        if let Ok(mut guard) = state_source.lock() {
            guard.filter.source_index = index;
        }
        if let Some(ui) = weak.upgrade() {
            refresh_cells(&ui, &state_source);
        }
    });

    let state_resolution = state.clone();
    let weak = ui.as_weak();
    ui.on_min_resolution_changed(move |index| {
        if let Ok(mut guard) = state_resolution.lock() {
            guard.filter.min_height = match index {
                1 => 1080,
                2 => 1440,
                3 => 2160,
                _ => 0,
            };
        }
        if let Some(ui) = weak.upgrade() {
            refresh_cells(&ui, &state_resolution);
        }
    });

    let state_sort = state.clone();
    let weak = ui.as_weak();
    ui.on_sort_changed(move |index| {
        if let Ok(mut guard) = state_sort.lock() {
            guard.filter.sort_index = index;
        }
        if let Some(ui) = weak.upgrade() {
            refresh_cells(&ui, &state_sort);
        }
    });

    // Re-scan cache + local sources without touching the running rotation.
    let weak = ui.as_weak();
    let state_refresh = state.clone();
    let cache_refresh = cache.clone();
    let sources_refresh = local_sources.clone();
    ui.on_refresh_requested(move || {
        spawn_scan(
            weak.clone(),
            state_refresh.clone(),
            cache_refresh.clone(),
            sources_refresh.clone(),
        );
    });

    ui.on_open_in_folder(|path| {
        if let Err(error) = reveal_in_file_manager(Path::new(path.as_str())) {
            warn!(error = %error, path = path.as_str(), "failed to reveal wallpaper");
        }
    });

    let weak = ui.as_weak();
    ui.on_close_window(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = ui.hide();
        }
    });

    let weak = ui.as_weak();
    let state_fetch = state.clone();
    let cache_fetch = cache.clone();
    let sources_fetch = local_sources.clone();
    let tx_fetch = tray_event_tx.clone();
    ui.on_fetch_requested(
        move |query, count, category, purity, sorting, atleast| {
            let request = build_fetch_request(
                &default_wallhaven,
                query.to_string(),
                count,
                category,
                purity,
                sorting,
                atleast,
            );
            spawn_fetch(
                weak.clone(),
                state_fetch.clone(),
                cache_fetch.clone(),
                sources_fetch.clone(),
                tx_fetch.clone(),
                request,
            );
        },
    );

    spawn_scan(
        ui.as_weak(),
        state.clone(),
        cache.clone(),
        local_sources.clone(),
    );

    ui.run().context("wallpaper picker event loop failed")?;
    Ok(())
}

/// Mirror the monitor selection into the UI model.
fn push_monitors(ui: &WallpaperPickerWindow, state: &PickerState) {
    let chips: Vec<MonitorChip> = state
        .monitor_names
        .iter()
        .zip(state.monitors.iter())
        .map(|(name, selected)| MonitorChip {
            name: SharedString::from(name.as_str()),
            selected: *selected,
        })
        .collect();
    let selected = state.monitors.iter().filter(|value| **value).count() as i32;
    let model: ModelRc<MonitorChip> = std::rc::Rc::new(VecModel::from(chips)).into();
    ui.set_monitors(model);
    ui.set_selected_monitor_count(selected);
    ui.set_monitor_chip_width(monitor_chip_width(&state.monitor_names));
}

/// Width for the display chips in the picker footer.
///
/// The backend reports real display names now, and their length varies a lot
/// (`P27QBD-RG` versus `显示器 2`). Slint cannot measure text from here, so each
/// label is measured conservatively — CJK glyphs occupy a full 12px cell,
/// everything else roughly half — and the widest one sets a shared width so the
/// row stays visually aligned.
fn monitor_chip_width(names: &[String]) -> f32 {
    const MIN_WIDTH: f32 = 96.0;
    const PADDING: f32 = 30.0;

    names
        .iter()
        .map(|name| {
            let text: f32 = name
                .chars()
                .map(|character| if character.is_ascii() { 6.8 } else { 12.0 })
                .sum();
            text + PADDING
        })
        .fold(MIN_WIDTH, f32::max)
}

/// Probe every known wallpaper on a worker thread and stream the results into
/// the grid in small batches.
fn spawn_scan(
    ui_weak: slint::Weak<WallpaperPickerWindow>,
    state: Arc<Mutex<PickerState>>,
    cache: Arc<CacheManager>,
    local_sources: Vec<SourceConfig>,
) {
    let ui_loading = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_loading.upgrade() {
            ui.set_loading(true);
        }
    });

    thread::spawn(move || {
        let mut paths: Vec<(PathBuf, bool)> = Vec::new();
        // Local sources first so the user's own wallpapers fill the grid
        // immediately, with remote cache images streaming in afterwards.
        paths.extend(
            collect_local_images(&local_sources)
                .into_iter()
                .map(|path| (path, true)),
        );
        match cache.list_remote_images() {
            Ok(images) => paths.extend(images.into_iter().map(|path| (path, false))),
            Err(error) => warn!(error = %error, "failed to list remote cache images"),
        }

        let mut batch: Vec<(PickerEntry, Option<(u32, u32, Vec<u8>)>)> =
            Vec::with_capacity(PROGRESS_BATCH);
        let mut total = 0usize;
        let mut reset = true;
        for (path, local) in paths {
            let Some((entry, thumb_rgba)) = probe_entry(&path, local) else {
                continue;
            };
            batch.push((entry, thumb_rgba));
            total += 1;
            if batch.len() >= PROGRESS_BATCH {
                push_batch(
                    &ui_weak,
                    &state,
                    std::mem::take(&mut batch),
                    reset,
                    false,
                );
                reset = false;
            }
        }
        info!(count = total, "wallpaper picker scanned available wallpapers");
        push_batch(&ui_weak, &state, batch, reset, true);
    });
}

/// Merge a batch of entries into the shared list and refresh the grid on the
/// UI thread; `done` also clears the loading indicator.
fn push_batch(
    ui_weak: &slint::Weak<WallpaperPickerWindow>,
    state: &Arc<Mutex<PickerState>>,
    batch: Vec<(PickerEntry, Option<(u32, u32, Vec<u8>)>)>,
    reset: bool,
    done: bool,
) {
    let ui_weak = ui_weak.clone();
    let state = state.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = ui_weak.upgrade() else {
            return;
        };
        if let Ok(mut guard) = state.lock() {
            if reset {
                guard.entries.clear();
            }
            for (entry, thumb_rgba) in batch {
                // Build + cache the thumbnail image on the UI thread, but purely
                // from in-memory RGBA bytes when available — no file IO here.
                let _ = cached_thumbnail(&entry.thumb_path, thumb_rgba.as_ref());
                guard.entries.push(entry);
            }
        }
        refresh_cells(&ui, &state);
        if done {
            ui.set_loading(false);
        }
    });
}

/// Rebuild the `cells` model from the loaded entries, applying the current
/// filters and marking each entry as favorited.
fn refresh_cells(ui: &WallpaperPickerWindow, state: &Arc<Mutex<PickerState>>) {
    let (entries, favorites, search, favorites_only, source_index, min_height, sort_index) =
        match state.lock() {
            Ok(guard) => (
                guard.entries.clone(),
                guard.favorites.clone(),
                guard.filter.search.to_lowercase(),
                guard.filter.favorites_only,
                guard.filter.source_index,
                guard.filter.min_height,
                guard.filter.sort_index,
            ),
            Err(_) => return,
        };

    let total = entries.len() as i32;
    let mut visible: Vec<&PickerEntry> = entries
        .iter()
        .filter(|entry| {
            if favorites_only && !favorites.contains(entry.path.as_str()) {
                return false;
            }
            match source_index {
                1 if entry.local => return false,
                2 if !entry.local => return false,
                _ => {}
            }
            if min_height > 0 && entry.longest_edge() < min_height {
                return false;
            }
            if !search.is_empty()
                && !entry.name.to_lowercase().contains(&search)
                && !entry.path.to_lowercase().contains(&search)
            {
                return false;
            }
            true
        })
        .collect();

    sort_entries(&mut visible, sort_index);

    let mut cells: Vec<WallpaperCell> = Vec::with_capacity(visible.len());
    for entry in visible {
        let Some(thumb) = cached_thumbnail(&entry.thumb_path, None) else {
            continue;
        };
        cells.push(WallpaperCell {
            path: SharedString::from(entry.path.as_str()),
            thumb,
            name: SharedString::from(entry.name.as_str()),
            favorited: favorites.contains(entry.path.as_str()),
            meta: SharedString::from(entry.meta_text()),
            local: entry.local,
        });
    }

    ui.set_total_count(total);
    ui.set_shown_count(cells.len() as i32);
    let model: ModelRc<WallpaperCell> = std::rc::Rc::new(VecModel::from(cells)).into();
    ui.set_cells(model);
}

fn sort_entries(entries: &mut [&PickerEntry], sort_index: i32) {
    match sort_index {
        1 => entries.sort_by(|a, b| b.name.to_lowercase().cmp(&a.name.to_lowercase())),
        2 => entries.sort_by(|a, b| b.modified.cmp(&a.modified)),
        3 => entries.sort_by(|a, b| a.modified.cmp(&b.modified)),
        4 => entries.sort_by(|a, b| {
            b.longest_edge()
                .cmp(&a.longest_edge())
                .then_with(|| b.bytes.cmp(&a.bytes))
        }),
        5 => entries.sort_by(|a, b| b.bytes.cmp(&a.bytes)),
        _ => entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
    }
}

/// Load a thumbnail through the UI-thread cache, decoding it at most once.
fn cached_thumbnail(
    thumb_path: &str,
    rgba: Option<&(u32, u32, Vec<u8>)>,
) -> Option<SlintImage> {
    THUMB_CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(image) = cache.get(thumb_path) {
            return Some(image.clone());
        }
        // Build the image from in-memory RGBA bytes whenever we have them
        // (decoded on the scan worker), so the UI thread never does disk IO.
        let image = match rgba {
            Some((width, height, buffer)) => {
                let mut pixel_buffer = SharedPixelBuffer::<Rgba8Pixel>::new(*width, *height);
                for (destination, source) in pixel_buffer
                    .make_mut_slice()
                    .iter_mut()
                    .zip(buffer.chunks_exact(4))
                {
                    *destination = Rgba8Pixel {
                        r: source[0],
                        g: source[1],
                        b: source[2],
                        a: source[3],
                    };
                }
                SlintImage::from_rgba8(pixel_buffer)
            }
            None => SlintImage::load_from_path(Path::new(thumb_path)).ok()?,
        };
        cache.insert(thumb_path.to_string(), image.clone());
        Some(image)
    })
}

/// Read dimensions / size / mtime, make (or reuse) a thumbnail, and return the
/// entry together with the thumbnail's raw RGBA bytes. The bytes are decoded
/// here on the scan worker, never on the UI thread.
fn probe_entry(path: &Path, local: bool) -> Option<(PickerEntry, Option<(u32, u32, Vec<u8>)>)> {
    let (width, height) = image::image_dimensions(path).ok()?;
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let (thumb_path, thumb_rgba) = make_thumbnail(path)?;
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_default();

    Some((
        PickerEntry {
            path: path.to_string_lossy().into_owned(),
            name,
            thumb_path,
            local,
            width,
            height,
            bytes: metadata.len(),
            modified,
        },
        thumb_rgba,
    ))
}

/// Create (or reuse) a thumbnail file and return it together with the decoded
/// RGBA pixels. The decoding happens on the scan worker so the UI thread only
/// ever does an in-memory copy when building the `slint::Image`.
fn make_thumbnail(path: &Path) -> Option<(String, Option<(u32, u32, Vec<u8>)>)> {
    let dir = std::env::temp_dir().join("aura-wallpaper-thumbs");
    std::fs::create_dir_all(&dir).ok()?;
    let key = blake3::hash(path.to_string_lossy().as_bytes())
        .to_hex()
        .to_string();
    let output = dir.join(format!("{key}.png"));

    let rgba = if !output.exists() {
        let source = image::open(path).ok()?;
        let (width, height) = image::GenericImageView::dimensions(&source);
        let scale = THUMB_WIDTH as f32 / width as f32;
        let target_height = (height as f32 * scale).max(1.0) as u32;
        let thumbnail = source.thumbnail(THUMB_WIDTH, target_height);
        match thumbnail.save(&output) {
            Ok(()) => {
                let rgba8 = thumbnail.into_rgba8();
                let (w, h) = (rgba8.width(), rgba8.height());
                Some((w, h, rgba8.into_raw()))
            }
            Err(error) => {
                warn!(error = %error, path = %path.display(), "failed to save wallpaper thumbnail");
                None
            }
        }
    } else {
        match image::open(&output) {
            Ok(image) => {
                let rgba8 = image.into_rgba8();
                let (w, h) = (rgba8.width(), rgba8.height());
                Some((w, h, rgba8.into_raw()))
            }
            Err(error) => {
                warn!(error = %error, path = %output.display(), "failed to read cached wallpaper thumbnail");
                None
            }
        }
    };

    Some((output.to_string_lossy().into_owned(), rgba))
}

fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let value = bytes as f64;
    if value >= KB * KB * KB {
        format!("{:.1} GB", value / (KB * KB * KB))
    } else if value >= KB * KB {
        format!("{:.1} MB", value / (KB * KB))
    } else if value >= KB {
        format!("{:.0} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Turn the fetch panel controls into a concrete Wallhaven query.
fn build_fetch_request(
    default_wallhaven: &Option<SourceConfig>,
    query: String,
    count: i32,
    category: i32,
    purity: i32,
    sorting: i32,
    atleast: i32,
) -> FetchRequest {
    let (default_top_range, default_api_key) = match default_wallhaven {
        Some(SourceConfig::Wallhaven {
            top_range, api_key, ..
        }) => (top_range.clone(), api_key.clone()),
        _ => (None, None),
    };

    let query = query.trim().to_string();
    FetchRequest {
        query: if query.is_empty() { None } else { Some(query) },
        categories: match category {
            1 => "general".to_string(),
            2 => "people".to_string(),
            3 => "anime".to_string(),
            _ => "general,people,anime".to_string(),
        },
        // Wallhaven expects a bit flag string: 1xx = sfw / sketchy / nsfw.
        purity: match purity {
            1 => "110".to_string(),
            2 => "111".to_string(),
            _ => "100".to_string(),
        },
        sorting: match sorting {
            1 => "date_added".to_string(),
            2 => "toplist".to_string(),
            3 => "views".to_string(),
            4 => "favorites".to_string(),
            _ => "random".to_string(),
        },
        top_range: default_top_range.unwrap_or_else(|| "1M".to_string()),
        atleast: match atleast {
            1 => Some("1920x1080".to_string()),
            2 => Some("2560x1440".to_string()),
            3 => Some("3840x2160".to_string()),
            _ => None,
        },
        max_items: count.clamp(1, 96) as usize,
        api_key: default_api_key,
    }
}

/// Download new wallpapers into the shared Wallhaven cache directory, then
/// re-scan the grid so they appear immediately.
fn spawn_fetch(
    ui_weak: slint::Weak<WallpaperPickerWindow>,
    state: Arc<Mutex<PickerState>>,
    cache: Arc<CacheManager>,
    local_sources: Vec<SourceConfig>,
    tray_event_tx: UnboundedSender<TrayEvent>,
    request: FetchRequest,
) {
    set_fetching(&ui_weak, true, "正在查询 Wallhaven…".to_string());

    thread::spawn(move || {
        let download_dir = match cache.ensure_remote_source_dir("wallhaven") {
            Ok(dir) => dir,
            Err(error) => {
                set_fetching(&ui_weak, false, format!("无法准备缓存目录：{error}"));
                return;
            }
        };

        let source_config = SourceConfig::Wallhaven {
            query: request.query.clone(),
            categories: Some(request.categories),
            purity: Some(request.purity),
            sorting: Some(request.sorting),
            top_range: Some(request.top_range),
            atleast: request.atleast,
            max_items: request.max_items,
            api_key: request.api_key,
        };

        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                set_fetching(&ui_weak, false, format!("无法启动下载运行时：{error}"));
                return;
            }
        };

        let outcome: Result<(usize, usize)> = runtime.block_on(async {
            let mut source = WallhavenSource::new(&source_config, download_dir)?;
            let candidates = source.refresh().await?;
            let found = candidates.len();
            let mut downloaded = 0usize;
            for candidate in &candidates {
                match candidate.prefetch().await {
                    Ok(_) => {
                        downloaded += 1;
                        let message = format!("已下载 {downloaded}/{found} 张…");
                        set_fetching(&ui_weak, true, message);
                    }
                    Err(error) => {
                        warn!(error = %error, "failed to download wallhaven wallpaper");
                    }
                }
            }
            Ok((found, downloaded))
        });

        match outcome {
            Ok((found, downloaded)) if found > 0 => {
                info!(found, downloaded, "wallhaven fetch finished");
                set_fetching(
                    &ui_weak,
                    false,
                    format!("完成：新下载 {downloaded} 张，正在刷新列表…"),
                );
                // Re-scan so the new files land in the grid, then let the main
                // loop rebuild its source pool so they join the rotation too.
                spawn_scan(
                    ui_weak.clone(),
                    state.clone(),
                    cache.clone(),
                    local_sources.clone(),
                );
                let _ = tray_event_tx.send(TrayEvent::ReloadSettings);
            }
            Ok(_) => {
                set_fetching(&ui_weak, false, "没有匹配的壁纸，换个关键词或分类试试".to_string());
            }
            Err(error) => {
                warn!(error = %error, "wallhaven fetch failed");
                set_fetching(
                    &ui_weak,
                    false,
                    format!("拉取失败：{error}（若站点无法直连，请在设置里配置 proxy）"),
                );
            }
        }
    });
}

fn set_fetching(ui_weak: &slint::Weak<WallpaperPickerWindow>, fetching: bool, status: String) {
    let ui_weak = ui_weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_fetching(fetching);
            ui.set_fetch_status(SharedString::from(status));
        }
    });
}

#[cfg(windows)]
fn reveal_in_file_manager(path: &Path) -> Result<()> {
    std::process::Command::new("explorer")
        .arg(format!("/select,{}", path.display()))
        .spawn()
        .with_context(|| format!("failed to open explorer for {}", path.display()))?;
    Ok(())
}

#[cfg(not(windows))]
fn reveal_in_file_manager(_path: &Path) -> Result<()> {
    anyhow::bail!("revealing files is only supported on Windows")
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

    fn test_state(entries: Vec<PickerEntry>, favorites: Vec<String>) -> Arc<Mutex<PickerState>> {
        Arc::new(Mutex::new(PickerState {
            entries,
            favorites: favorites.into_iter().collect(),
            filter: PickerFilter::default(),
            monitor_names: vec!["显示器 1".into(), "显示器 2".into()],
            monitors: vec![false, false],
        }))
    }

    fn make_entry(dir: &Path, name: &str, width: u32, height: u32, bytes: u64, modified: u64) -> PickerEntry {
        let thumb = dir.join(format!("thumb-{name}.png"));
        image::RgbaImage::from_pixel(4, 4, image::Rgba([20, 40, 60, 255]))
            .save(&thumb)
            .unwrap();
        PickerEntry {
            path: dir.join(format!("wallpaper-{name}")).to_string_lossy().into_owned(),
            name: name.to_string(),
            thumb_path: thumb.to_string_lossy().into_owned(),
            local: false,
            width,
            height,
            bytes,
            modified,
        }
    }

    /// The picker component must instantiate, accept a thumbnail model, and
    /// apply search / source / resolution / sort filters. Runs on the shared
    /// Slint test thread because winit only allows a single platform/event
    /// loop per process.
    #[test]
    fn picker_window_filters_entries() {
        crate::settings::run_slint_test(|| {
            use slint::Model;

            let ui = WallpaperPickerWindow::new().unwrap();
            ui.on_apply_all(|_| {});
            ui.on_apply_to_monitor(|_, _| {});
            ui.on_apply_to_selection(|_| {});
            ui.on_add_favorite(|_| {});
            ui.on_search_changed(|_| {});
            ui.on_favorites_toggled(|_| {});
            ui.on_source_changed(|_| {});
            ui.on_min_resolution_changed(|_| {});
            ui.on_sort_changed(|_| {});
            ui.on_toggle_monitor(|_| {});
            ui.on_select_all_monitors(|_| {});
            ui.on_refresh_requested(|| {});
            ui.on_fetch_requested(|_, _, _, _, _, _| {});
            ui.on_open_in_folder(|_| {});
            ui.on_close_window(|| {});
            ui.on_confirm_delete(|_| {});

            let dir = std::env::temp_dir().join("aura-picker-filter-test");
            std::fs::create_dir_all(&dir).unwrap();

            let mut remote_large = make_entry(&dir, "aurora.png", 3840, 2160, 4_000_000, 300);
            let mut remote_small = make_entry(&dir, "beach.png", 1280, 720, 120_000, 100);
            let mut local_image = make_entry(&dir, "trip.png", 2560, 1440, 900_000, 200);
            remote_large.local = false;
            remote_small.local = false;
            local_image.local = true;

            let state = test_state(
                vec![remote_large, remote_small, local_image],
                vec![
                    dir.join("wallpaper-beach.png").to_string_lossy().into_owned(),
                ],
            );

            // Everything, newest first.
            refresh_cells(&ui, &state);
            assert_eq!(ui.get_total_count(), 3);
            assert_eq!(ui.get_shown_count(), 3);
            assert_eq!(ui.get_cells().row_data(0).unwrap().name, "aurora.png");

            // Search narrows by name.
            state.lock().unwrap().filter.search = "beach".into();
            refresh_cells(&ui, &state);
            assert_eq!(ui.get_shown_count(), 1);
            assert_eq!(ui.get_cells().row_count(), 1);

            // Favorites-only keeps the single favorited entry.
            state.lock().unwrap().filter.search.clear();
            state.lock().unwrap().filter.favorites_only = true;
            refresh_cells(&ui, &state);
            assert_eq!(ui.get_shown_count(), 1);
            assert!(ui.get_cells().row_data(0).unwrap().favorited);

            // Local-only source filter.
            state.lock().unwrap().filter.favorites_only = false;
            state.lock().unwrap().filter.source_index = 2;
            refresh_cells(&ui, &state);
            assert_eq!(ui.get_shown_count(), 1);
            assert!(ui.get_cells().row_data(0).unwrap().local);

            // Minimum resolution of 1440p drops the 720p image but keeps 4K.
            state.lock().unwrap().filter.source_index = 0;
            state.lock().unwrap().filter.min_height = 1440;
            refresh_cells(&ui, &state);
            assert_eq!(ui.get_shown_count(), 2);

            // Sorting by file size puts the 4 MB image first.
            state.lock().unwrap().filter.min_height = 0;
            state.lock().unwrap().filter.sort_index = 5;
            refresh_cells(&ui, &state);
            assert_eq!(ui.get_cells().row_data(0).unwrap().name, "aurora.png");

            // No match clears the grid.
            state.lock().unwrap().filter.sort_index = 2;
            state.lock().unwrap().filter.search = "no-match".into();
            refresh_cells(&ui, &state);
            assert_eq!(ui.get_shown_count(), 0);
            assert_eq!(ui.get_cells().row_count(), 0);

            let _ = ui.hide();
            std::fs::remove_dir_all(&dir).ok();
        });
    }

    #[test]
    fn monitor_chip_width_covers_the_widest_label() {
        assert_eq!(monitor_chip_width(&[]), 96.0, "empty input keeps the floor");
        assert_eq!(monitor_chip_width(&[String::new()]), 96.0);

        // A device name is wider than the floor...
        let device = monitor_chip_width(&["LEN160-3.2K".to_string()]);
        assert!(device > 96.0 && device < 140.0, "unexpected width {device}");

        // ...and CJK glyphs occupy a full cell, not an ASCII one.
        assert!(
            monitor_chip_width(&["显示器屏幕名称".to_string()])
                > monitor_chip_width(&["abcdefg".to_string()]),
            "CJK glyphs must not be measured like ASCII"
        );

        // The widest label sets the shared width.
        let mixed = vec!["短".to_string(), "LEN160-3.2K".to_string()];
        assert_eq!(
            monitor_chip_width(&mixed),
            monitor_chip_width(&["LEN160-3.2K".to_string()])
        );
    }

    /// The display chooser must round-trip through the monitor model.
    #[test]
    fn picker_monitor_selection_round_trips() {
        crate::settings::run_slint_test(|| {
            use slint::Model;

            let ui = WallpaperPickerWindow::new().unwrap();
            let state = test_state(Vec::new(), Vec::new());

            push_monitors(&ui, &state.lock().unwrap());
            assert_eq!(ui.get_monitors().row_count(), 2);
            assert_eq!(ui.get_selected_monitor_count(), 0);

            state.lock().unwrap().monitors = vec![true, true];
            push_monitors(&ui, &state.lock().unwrap());
            assert_eq!(ui.get_selected_monitor_count(), 2);
            assert!(ui.get_monitors().row_data(0).unwrap().selected);

            let _ = ui.hide();
        });
    }

    /// Every fetch-panel combination must map to a valid Wallhaven query.
    #[test]
    fn fetch_request_maps_panel_controls() {
        let request = build_fetch_request(&None, "  aurora ".into(), 48, 2, 1, 4, 2);

        assert_eq!(request.query.as_deref(), Some("aurora"));
        assert_eq!(request.categories, "people");
        assert_eq!(request.purity, "110");
        assert_eq!(request.sorting, "favorites");
        assert_eq!(request.atleast.as_deref(), Some("2560x1440"));
        assert_eq!(request.max_items, 48);

        // Blank query means "no keyword" and the count is clamped.
        let fallback = build_fetch_request(&None, "   ".into(), 5000, 0, 0, 0, 0);
        assert!(fallback.query.is_none());
        assert_eq!(fallback.categories, "general,people,anime");
        assert_eq!(fallback.purity, "100");
        assert_eq!(fallback.sorting, "random");
        assert!(fallback.atleast.is_none());
        assert_eq!(fallback.max_items, 96);
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

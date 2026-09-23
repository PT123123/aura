//! Slint-based settings window (Windows only).
//!
//! The tray "Settings" item previously handed the `.hcl` config file to the
//! shell (`ShellExecuteW`), which on machines without an `.hcl` association
//! opens the "how do you want to open this file?" dialog. This module replaces
//! that flow on Windows with a real settings window: values are loaded from
//! the HCL config, edited visually, and written back as HCL while preserving
//! any unknown/extra keys.

use crate::config::{
    AuraConfig, OutputFormat, RendererMode, ShaderColorSpace, ShaderDesktopScope, SourceConfig,
};
use crate::errors::Result;
use crate::tray::TrayEvent;
use anyhow::Context;
use slint::ComponentHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

slint::include_modules!();

static SETTINGS_WINDOW_OPEN: AtomicBool = AtomicBool::new(false);

/// Slint/winit binds the platform to the first thread that creates a window,
/// so every test that constructs a Slint window must run on one shared worker
/// thread. Panics are forwarded back to the calling test thread.
#[cfg(test)]
pub(crate) fn run_slint_test(test: impl FnOnce() + Send + 'static) {
    use std::panic::AssertUnwindSafe;
    use std::sync::mpsc;
    use std::sync::OnceLock;

    type Job = Box<dyn FnOnce() + Send>;
    static QUEUE: OnceLock<mpsc::Sender<Job>> = OnceLock::new();

    let tx = QUEUE.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::spawn(move || {
            while let Ok(job) = rx.recv() {
                job();
            }
        });
        tx
    });

    let (done_tx, done_rx) = mpsc::channel();
    tx.send(Box::new(move || {
        let result = std::panic::catch_unwind(AssertUnwindSafe(test));
        let _ = done_tx.send(result);
    }))
    .expect("slint test worker thread has exited");

    match done_rx.recv().expect("slint test worker thread has exited") {
        Ok(()) => {}
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

pub fn open_settings_window(config_path: PathBuf, reload_tx: UnboundedSender<TrayEvent>) {
    if SETTINGS_WINDOW_OPEN.swap(true, Ordering::SeqCst) {
        info!("settings window is already open; ignoring duplicate request");
        return;
    }
    thread::spawn(move || {
        let result = run_settings_window(&config_path, &reload_tx);
        SETTINGS_WINDOW_OPEN.store(false, Ordering::SeqCst);
        if let Err(error) = result {
            warn!(error = %error, "settings window failed");
        }
    });
}

fn run_settings_window(config_path: &Path, reload_tx: &UnboundedSender<TrayEvent>) -> Result<()> {
    let ui = SettingsWindow::new().context("failed to create settings window")?;

    let config_text = std::fs::read_to_string(config_path)
        .with_context(|| format!("failed to read config file {}", config_path.display()))?;
    let parsed = crate::config::parse_from_str_with_warnings(&config_text, config_path);
    match parsed {
        Ok(loaded) => fill_ui_from_config(&ui, &loaded.config),
        Err(error) => {
            warn!(error = %error, "config parse failed; settings window shows defaults");
            ui.set_status_text("配置解析失败，显示默认值。".into());
        }
    }

    let save_path = config_path.to_path_buf();
    let save_reload_tx = reload_tx.clone();
    let weak = ui.as_weak();
    ui.on_save(move || {
        if let Some(ui) = weak.upgrade() {
            match save_config_from_ui(&ui, &save_path) {
                Ok(()) => {
                    let _ = save_reload_tx.send(TrayEvent::ReloadSettings);
                    ui.set_status_text("已保存并重载配置。".into());
                }
                Err(error) => {
                    warn!(error = %error, "failed to save settings");
                    ui.set_status_text(format!("保存失败: {error:#}").into());
                }
            }
        }
    });

    let reload_path = config_path.to_path_buf();
    let weak = ui.as_weak();
    ui.on_reload_settings(move || {
        if let Some(ui) = weak.upgrade() {
            match reload_ui_from_disk(&ui, &reload_path) {
                Ok(()) => ui.set_status_text("已重新载入配置。".into()),
                Err(error) => {
                    warn!(error = %error, "failed to reload settings into window");
                    ui.set_status_text(format!("重载失败: {error:#}").into());
                }
            }
        }
    });

    let open_path = config_path.to_path_buf();
    let weak = ui.as_weak();
    ui.on_open_config_file(move || {
        if let Some(ui) = weak.upgrade() {
            if let Err(error) = crate::tray::open_settings(&open_path) {
                warn!(error = %error, "failed to open config file externally");
                ui.set_status_text(format!("打开配置文件失败: {error:#}").into());
            }
        }
    });

    ui.run().context("settings window event loop failed")?;
    Ok(())
}

fn reload_ui_from_disk(ui: &SettingsWindow, config_path: &Path) -> Result<()> {
    let config_text = std::fs::read_to_string(config_path)
        .with_context(|| format!("failed to read config file {}", config_path.display()))?;
    let loaded = crate::config::parse_from_str_with_warnings(&config_text, config_path)?;
    fill_ui_from_config(ui, &loaded.config);
    Ok(())
}

fn fill_ui_from_config(ui: &SettingsWindow, config: &AuraConfig) {
    ui.set_renderer_index(match config.renderer {
        RendererMode::Image => 0,
        RendererMode::Shader => 1,
    });
    ui.set_image_timer_secs(secs_to_i32(config.image.timer));
    ui.set_remote_update_secs(secs_to_i32(config.image.remote_update_timer));
    ui.set_format_index(match config.image.format {
        OutputFormat::Png => 0,
        OutputFormat::Jpg => 1,
    });
    ui.set_jpeg_quality(config.image.jpeg_quality as i32);
    ui.set_sources_summary(source_summary(&config.image.sources).into());
    ui.set_updater_enabled(config.updater.enabled);
    ui.set_updater_interval_secs(secs_to_i32(config.updater.check_interval));
    ui.set_feed_url(config.updater.feed_url.clone().into());
    if let Some(shader) = &config.shader {
        ui.set_shader_name(shader.name.clone().into());
        ui.set_shader_fps(shader.target_fps as i32);
        ui.set_shader_resolution(shader.resolution as i32);
        ui.set_shader_mouse_enabled(shader.mouse_enabled);
        ui.set_shader_scope_index(match shader.desktop_scope {
            ShaderDesktopScope::Virtual => 0,
            ShaderDesktopScope::Primary => 1,
        });
        ui.set_shader_color_space_index(match shader.color_space {
            ShaderColorSpace::Unorm => 0,
            ShaderColorSpace::Srgb => 1,
        });
    }
    ui.set_log_level_index(match config.log_level.as_str() {
        "error" => 0,
        "warn" => 1,
        "info" => 2,
        "debug" => 3,
        "trace" => 4,
        _ => 2,
    });
    ui.set_cache_dir(config.cache_dir.to_string_lossy().into_owned().into());
    ui.set_state_file(config.state_file.to_string_lossy().into_owned().into());
    ui.set_proxy_url(config.proxy.clone().unwrap_or_default().into());
    fill_wallhaven_from_config(ui, config);
}

fn parse_list(value: &Option<String>) -> Vec<String> {
    value
        .iter()
        .flat_map(|v| v.split(','))
        .map(|part| part.trim().to_ascii_lowercase())
        .filter(|part| !part.is_empty())
        .collect()
}

fn fill_wallhaven_from_config(ui: &SettingsWindow, config: &AuraConfig) {
    let found = config.image.sources.iter().find_map(|source| match source {
        SourceConfig::Wallhaven {
            query,
            categories,
            purity,
            sorting,
            top_range,
            atleast,
            max_items,
            api_key,
        } => Some((
            query.clone(),
            categories.clone(),
            purity.clone(),
            sorting.clone(),
            top_range.clone(),
            atleast.clone(),
            *max_items,
            api_key.clone(),
        )),
        _ => None,
    });

    let (query, categories, purity, sorting, top_range, atleast, max_items, api_key) =
        match found {
            Some(values) => values,
            None => {
                ui.set_wallhaven_enabled(false);
                ui.set_wallhaven_query("".into());
                ui.set_wallhaven_cat_general(true);
                ui.set_wallhaven_cat_people(true);
                ui.set_wallhaven_cat_anime(true);
                ui.set_wallhaven_purity_sfw(true);
                ui.set_wallhaven_purity_sketchy(false);
                ui.set_wallhaven_purity_nsfw(false);
                ui.set_wallhaven_sorting_index(0);
                ui.set_wallhaven_top_range_index(3);
                ui.set_wallhaven_atleast_index(0);
                ui.set_wallhaven_max_items(24);
                ui.set_wallhaven_api_key("".into());
                return;
            }
        };

    ui.set_wallhaven_enabled(true);
    ui.set_wallhaven_query(query.unwrap_or_default().into());
    let cats = parse_list(&categories);
    ui.set_wallhaven_cat_general(cats.contains(&"general".to_string()));
    ui.set_wallhaven_cat_people(cats.contains(&"people".to_string()));
    ui.set_wallhaven_cat_anime(cats.contains(&"anime".to_string()));
    let purity_values = parse_list(&purity);
    ui.set_wallhaven_purity_sfw(purity_values.contains(&"sfw".to_string()));
    ui.set_wallhaven_purity_sketchy(purity_values.contains(&"sketchy".to_string()));
    ui.set_wallhaven_purity_nsfw(purity_values.contains(&"nsfw".to_string()));
    ui.set_wallhaven_sorting_index(match sorting.as_deref() {
        Some("relevance") => 1,
        Some("random") => 2,
        Some("views") => 3,
        Some("favorites") => 4,
        Some("toplist") => 5,
        _ => 0,
    });
    ui.set_wallhaven_top_range_index(match top_range.as_deref() {
        Some("1d") => 0,
        Some("3d") => 1,
        Some("1w") => 2,
        Some("3M") => 4,
        Some("6M") => 5,
        Some("1y") => 6,
        _ => 3,
    });
    ui.set_wallhaven_atleast_index(match atleast.as_deref() {
        Some("1920x1080") => 1,
        Some("2560x1440") => 2,
        Some("3840x2160") => 3,
        _ => 0,
    });
    ui.set_wallhaven_max_items(max_items.min(1000) as i32);
    ui.set_wallhaven_api_key(api_key.unwrap_or_default().into());
}

fn secs_to_i32(duration: std::time::Duration) -> i32 {
    duration.as_secs().min(i32::MAX as u64) as i32
}

fn source_summary(sources: &[SourceConfig]) -> String {
    if sources.is_empty() {
        return "（无来源）".to_string();
    }
    let mut lines = Vec::with_capacity(sources.len());
    for (index, source) in sources.iter().enumerate() {
        let line = match source {
            SourceConfig::File { path } => format!("{}. 文件: {}", index + 1, path.display()),
            SourceConfig::Directory {
                path,
                recursive,
                extensions,
            } => {
                let ext = extensions
                    .as_ref()
                    .map(|values| format!(" ({})", values.join(", ")))
                    .unwrap_or_default();
                format!(
                    "{}. 目录: {}{}（递归: {}）",
                    index + 1,
                    path.display(),
                    ext,
                    if *recursive { "是" } else { "否" }
                )
            }
            SourceConfig::Rss {
                url, max_items, ..
            } => format!("{}. RSS: {}（最多 {} 项）", index + 1, url, max_items),
            SourceConfig::Wallhaven {
                query,
                categories,
                purity,
                sorting,
                max_items,
                ..
            } => {
                let query = query.clone().unwrap_or_default();
                let query = if query.is_empty() { "全部".to_string() } else { query };
                format!(
                    "{}. Wallhaven: {}（{} / {} / {}，最多 {} 项）",
                    index + 1,
                    query,
                    categories.as_deref().unwrap_or("全部"),
                    purity.as_deref().unwrap_or("sfw"),
                    sorting.as_deref().unwrap_or("date_added"),
                    max_items
                )
            }
        };
        lines.push(line);
    }
    lines.join("\n")
}

fn save_config_from_ui(ui: &SettingsWindow, config_path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(config_path)
        .with_context(|| format!("failed to read config file {}", config_path.display()))?;
    let mut root: hcl::Value = hcl::from_str(&text)
        .with_context(|| format!("failed to parse config file {}", config_path.display()))?;

    set_object_string(&mut root, "", "renderer", renderer_str(ui.get_renderer_index()))?;
    set_object_number(&mut root, "image", "timer", ui.get_image_timer_secs() as i64)?;
    set_object_number(
        &mut root,
        "image",
        "remoteUpdateTimer",
        ui.get_remote_update_secs() as i64,
    )?;
    set_object_string(&mut root, "image", "format", format_str(ui.get_format_index()))?;
    set_object_number(&mut root, "image", "jpeg_quality", ui.get_jpeg_quality() as i64)?;

    let shader = ensure_object(&mut root, "shader")?;
    set_in_object_string(shader, "name", &ui.get_shader_name())?;
    set_in_object_number(shader, "target_fps", ui.get_shader_fps() as i64)?;
    set_in_object_number(shader, "resolution", ui.get_shader_resolution() as i64)?;
    set_in_object_bool(shader, "mouse_enabled", ui.get_shader_mouse_enabled())?;
    set_in_object_string(
        shader,
        "desktop_scope",
        scope_str(ui.get_shader_scope_index()),
    )?;
    set_in_object_string(
        shader,
        "color_space",
        color_space_str(ui.get_shader_color_space_index()),
    )?;

    set_object_bool(&mut root, "updater", "enabled", ui.get_updater_enabled())?;
    set_object_number(
        &mut root,
        "updater",
        "checkInterval",
        ui.get_updater_interval_secs() as i64,
    )?;
    set_object_string(&mut root, "updater", "feedUrl", &ui.get_feed_url())?;
    set_object_string(&mut root, "", "log_level", log_level_str(ui.get_log_level_index()))?;

    // An empty proxy field removes the key entirely so the environment
    // variables stay in charge instead of an empty override.
    let proxy = ui.get_proxy_url().trim().to_string();
    let root_map = ensure_object(&mut root, "")?;
    if proxy.is_empty() {
        root_map.shift_remove("proxy");
    } else {
        root_map.insert("proxy".to_string(), hcl::Value::from(proxy.as_str()));
    }

    save_wallhaven_to_root(&mut root, ui)?;

    let serialized = hcl::to_string(&root).context("failed to serialize config")?;
    std::fs::write(config_path, serialized)
        .with_context(|| format!("failed to write config file {}", config_path.display()))?;
    Ok(())
}

fn renderer_str(index: i32) -> &'static str {
    match index {
        1 => "shader",
        _ => "image",
    }
}

fn format_str(index: i32) -> &'static str {
    match index {
        1 => "jpg",
        _ => "png",
    }
}

fn scope_str(index: i32) -> &'static str {
    match index {
        1 => "primary",
        _ => "virtual",
    }
}

fn color_space_str(index: i32) -> &'static str {
    match index {
        1 => "srgb",
        _ => "unorm",
    }
}

fn log_level_str(index: i32) -> &'static str {
    match index {
        0 => "error",
        1 => "warn",
        3 => "debug",
        4 => "trace",
        _ => "info",
    }
}

/// Returns the object map for `key` (empty string = root object), creating it
/// when absent.
fn ensure_object<'a>(root: &'a mut hcl::Value, key: &str) -> Result<&'a mut hcl::Map<String, hcl::Value>> {
    let map = root.as_object_mut().context("config root is not an object")?;
    if key.is_empty() {
        return Ok(map);
    }
    if !map.contains_key(key) {
        map.insert(key.to_string(), hcl::Value::from(hcl::Map::new()));
    }
    map.get_mut(key)
        .and_then(|value| value.as_object_mut())
        .with_context(|| format!("config key '{key}' is not an object"))
}

fn set_in_object_string(map: &mut hcl::Map<String, hcl::Value>, key: &str, value: &str) -> Result<()> {
    map.insert(key.to_string(), hcl::Value::from(value));
    Ok(())
}

fn set_in_object_number(map: &mut hcl::Map<String, hcl::Value>, key: &str, value: i64) -> Result<()> {
    map.insert(key.to_string(), hcl::Value::from(value));
    Ok(())
}

fn set_in_object_bool(map: &mut hcl::Map<String, hcl::Value>, key: &str, value: bool) -> Result<()> {
    map.insert(key.to_string(), hcl::Value::from(value));
    Ok(())
}

fn set_object_string(root: &mut hcl::Value, object: &str, key: &str, value: &str) -> Result<()> {
    set_in_object_string(ensure_object(root, object)?, key, value)
}

fn set_object_number(root: &mut hcl::Value, object: &str, key: &str, value: i64) -> Result<()> {
    set_in_object_number(ensure_object(root, object)?, key, value)
}

fn set_object_bool(root: &mut hcl::Value, object: &str, key: &str, value: bool) -> Result<()> {
    set_in_object_bool(ensure_object(root, object)?, key, value)
}

/// Synchronises the Wallhaven source entry inside `image.sources`.
///
/// When the UI toggle is enabled, an existing `type = "wallhaven"` entry is
/// updated (or appended); when disabled, any wallhaven entry is removed.
/// Other source entries are left untouched.
fn save_wallhaven_to_root(root: &mut hcl::Value, ui: &SettingsWindow) -> Result<()> {
    let enabled = ui.get_wallhaven_enabled();
    let image = ensure_object(root, "image")?;

    let had_sources = image.contains_key("sources");
    if !had_sources && !enabled {
        return Ok(());
    }
    if !had_sources {
        image.insert("sources".to_string(), hcl::Value::Array(Vec::new()));
    }

    let sources = image
        .get_mut("sources")
        .and_then(|value| value.as_array_mut())
        .context("image.sources is not an array")?;

    sources.retain(|value| {
        !(value
            .as_object()
            .and_then(|map| map.get("type"))
            .and_then(|kind| kind.as_str())
            == Some("wallhaven"))
    });

    if !enabled {
        return Ok(());
    }

    let mut entry = hcl::Map::new();
    entry.insert("type".to_string(), hcl::Value::from("wallhaven"));

    let query = ui.get_wallhaven_query().trim().to_string();
    if !query.is_empty() {
        entry.insert("query".to_string(), hcl::Value::from(query));
    }

    let mut categories = Vec::new();
    if ui.get_wallhaven_cat_general() {
        categories.push("general");
    }
    if ui.get_wallhaven_cat_people() {
        categories.push("people");
    }
    if ui.get_wallhaven_cat_anime() {
        categories.push("anime");
    }
    if !categories.is_empty() {
        entry.insert(
            "categories".to_string(),
            hcl::Value::from(categories.join(",")),
        );
    }

    let mut purity = Vec::new();
    if ui.get_wallhaven_purity_sfw() {
        purity.push("sfw");
    }
    if ui.get_wallhaven_purity_sketchy() {
        purity.push("sketchy");
    }
    if ui.get_wallhaven_purity_nsfw() {
        purity.push("nsfw");
    }
    if !purity.is_empty() {
        entry.insert("purity".to_string(), hcl::Value::from(purity.join(",")));
    }

    let sorting = match ui.get_wallhaven_sorting_index() {
        1 => "relevance",
        2 => "random",
        3 => "views",
        4 => "favorites",
        5 => "toplist",
        _ => "date_added",
    };
    entry.insert("sorting".to_string(), hcl::Value::from(sorting));

    if sorting == "toplist" {
        let top_range = match ui.get_wallhaven_top_range_index() {
            0 => "1d",
            1 => "3d",
            2 => "1w",
            4 => "3M",
            5 => "6M",
            6 => "1y",
            _ => "1M",
        };
        entry.insert("topRange".to_string(), hcl::Value::from(top_range));
    }

    let atleast = match ui.get_wallhaven_atleast_index() {
        1 => "1920x1080",
        2 => "2560x1440",
        3 => "3840x2160",
        _ => "",
    };
    if !atleast.is_empty() {
        entry.insert("atleast".to_string(), hcl::Value::from(atleast));
    }

    let max_items = ui.get_wallhaven_max_items().clamp(1, 1000);
    entry.insert("maxItems".to_string(), hcl::Value::from(max_items as i64));

    let api_key = ui.get_wallhaven_api_key().trim().to_string();
    if !api_key.is_empty() {
        entry.insert("apiKey".to_string(), hcl::Value::from(api_key));
    }

    sources.push(hcl::Value::from(entry));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field<'a>(root: &'a hcl::Value, path: &[&str]) -> Option<&'a hcl::Value> {
        let mut current = root;
        for key in path {
            current = current.as_object()?.get(*key)?;
        }
        Some(current)
    }

    #[test]
    fn hcl_roundtrip_updates_fields_and_preserves_unknown_keys() {
        let text = r#"
renderer = "image"

image {
    timer = 300
    remoteUpdateTimer = 3600
    format = "png"
    jpeg_quality = 90
    sources = [
        {
            type = "directory"
            path = "C:/Pictures"
            recursive = true
        }
    ]
    custom_unknown = "keep-me"
}

updater {
    enabled = true
    checkInterval = 21600
    feedUrl = "https://example.com/feed"
}

log_level = "info"
unknown_root_key = 42
"#;
        let mut root: hcl::Value = hcl::from_str(text).expect("parse should succeed");
        set_object_string(&mut root, "", "renderer", "shader").expect("set renderer");
        set_object_number(&mut root, "image", "timer", 600).expect("set timer");
        set_object_string(&mut root, "image", "format", "jpg").expect("set format");
        set_object_bool(&mut root, "updater", "enabled", false).expect("set updater");
        set_object_string(&mut root, "", "log_level", "debug").expect("set log level");
        let shader = ensure_object(&mut root, "shader").expect("ensure shader");
        set_in_object_string(shader, "name", "silk").expect("set shader name");
        set_in_object_number(shader, "target_fps", 30).expect("set shader fps");

        let out = hcl::to_string(&root).expect("serialize should succeed");
        let reparsed: hcl::Value = hcl::from_str(&out).expect("reparse should succeed");

        assert_eq!(field(&reparsed, &["renderer"]).and_then(|v| v.as_str()), Some("shader"));
        assert_eq!(field(&reparsed, &["image", "timer"]).and_then(|v| v.as_u64()), Some(600));
        assert_eq!(field(&reparsed, &["image", "format"]).and_then(|v| v.as_str()), Some("jpg"));
        assert_eq!(
            field(&reparsed, &["image", "custom_unknown"]).and_then(|v| v.as_str()),
            Some("keep-me"),
            "unknown image key must survive a round trip"
        );
        assert_eq!(
            field(&reparsed, &["updater", "enabled"]).and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(field(&reparsed, &["log_level"]).and_then(|v| v.as_str()), Some("debug"));
        assert_eq!(field(&reparsed, &["shader", "name"]).and_then(|v| v.as_str()), Some("silk"));
        assert_eq!(
            field(&reparsed, &["shader", "target_fps"]).and_then(|v| v.as_u64()),
            Some(30)
        );
        assert_eq!(field(&reparsed, &["unknown_root_key"]).and_then(|v| v.as_u64()), Some(42));
        assert!(field(&reparsed, &["image", "sources"]).is_some_and(|v| v.is_array()));
    }

    fn find_wallhaven_entry<'a>(root: &'a hcl::Value) -> Option<&'a hcl::Map<String, hcl::Value>> {
        let sources = root.as_object()?.get("image")?.as_object()?.get("sources")?.as_array()?;
        sources.iter().find_map(|value| {
            let map = value.as_object()?;
            if map.get("type")?.as_str() == Some("wallhaven") {
                Some(map)
            } else {
                None
            }
        })
    }

    #[test]
    fn wallhaven_save_writes_entry_when_enabled() {
        crate::settings::run_slint_test(|| {
            let ui = SettingsWindow::new().expect("settings window should be creatable in test");
            ui.set_wallhaven_enabled(true);
            ui.set_wallhaven_query("aurora".into());
            ui.set_wallhaven_cat_general(true);
            ui.set_wallhaven_cat_people(false);
            ui.set_wallhaven_cat_anime(true);
            ui.set_wallhaven_purity_sfw(true);
            ui.set_wallhaven_purity_sketchy(false);
            ui.set_wallhaven_purity_nsfw(false);
            ui.set_wallhaven_sorting_index(5);
            ui.set_wallhaven_top_range_index(3);
            ui.set_wallhaven_atleast_index(1);
            ui.set_wallhaven_max_items(10);
            ui.set_wallhaven_api_key("".into());

            let mut root: hcl::Value = hcl::from_str(
                r#"image = { sources = [ { type = "rss", url = "https://example.com/feed" } ] }"#,
            )
            .expect("parse should succeed");
            save_wallhaven_to_root(&mut root, &ui).expect("save should succeed");

            let entry = find_wallhaven_entry(&root).expect("wallhaven entry should exist");
            assert_eq!(entry.get("type").and_then(|v| v.as_str()), Some("wallhaven"));
            assert_eq!(entry.get("query").and_then(|v| v.as_str()), Some("aurora"));
            assert_eq!(entry.get("categories").and_then(|v| v.as_str()), Some("general,anime"));
            assert_eq!(entry.get("purity").and_then(|v| v.as_str()), Some("sfw"));
            assert_eq!(entry.get("sorting").and_then(|v| v.as_str()), Some("toplist"));
            assert_eq!(entry.get("topRange").and_then(|v| v.as_str()), Some("1M"));
            assert_eq!(entry.get("atleast").and_then(|v| v.as_str()), Some("1920x1080"));
            assert_eq!(entry.get("maxItems").and_then(|v| v.as_u64()), Some(10));
            assert!(!entry.contains_key("apiKey"));

            // RSS entry must be untouched
            let sources = root
                .as_object()
                .and_then(|m| m.get("image"))
                .and_then(|m| m.as_object())
                .and_then(|m| m.get("sources"))
                .and_then(|v| v.as_array())
                .expect("sources array");
            assert_eq!(sources.len(), 2);

            // now disable: wallhaven entry is removed, rss stays
            ui.set_wallhaven_enabled(false);
            save_wallhaven_to_root(&mut root, &ui).expect("save should succeed");
            assert!(find_wallhaven_entry(&root).is_none());
            let sources = root
                .as_object()
                .and_then(|m| m.get("image"))
                .and_then(|m| m.as_object())
                .and_then(|m| m.get("sources"))
                .and_then(|v| v.as_array())
                .expect("sources array");
            assert_eq!(sources.len(), 1);
            assert_eq!(
                sources[0].as_object().and_then(|m| m.get("type")).and_then(|v| v.as_str()),
                Some("rss")
            );
        });
    }
}

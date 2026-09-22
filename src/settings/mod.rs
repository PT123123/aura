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
}

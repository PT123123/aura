use crate::errors::Result;
use anyhow::{bail, Context};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_TIMER_SECS: u64 = 300;
const DEFAULT_REMOTE_UPDATE_TIMER_SECS: u64 = 3600;
const DEFAULT_UPDATER_CHECK_INTERVAL_SECS: u64 = 6 * 3600;
const MIN_TIMER_SECS: u64 = 5;
const MIN_REMOTE_UPDATE_SECS: u64 = 30;
const MIN_UPDATER_CHECK_INTERVAL_SECS: u64 = 10 * 60;
const DEFAULT_JPEG_QUALITY: u8 = 90;
const DEFAULT_SHADER_TARGET_FPS: u16 = 60;
const DEFAULT_SHADER_RESOLUTION_PERCENT: u8 = 100;
const DEFAULT_SHADER_NAME: &str = "gradient_glossy";
const LEGACY_SHADER_NAME: &str = "gradient_shader";
const DEFAULT_SHADER_COLOR_SPACE: ShaderColorSpace = ShaderColorSpace::Unorm;
const DEFAULT_MAX_CACHE_MB: u64 = 1024;
const DEFAULT_MAX_CACHE_AGE_DAYS: u64 = 30;
const DEFAULT_UPDATER_FEED_URL: &str = "https://github.com/hmerritt/aura/releases/latest/download";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Jpg,
    Png,
}

impl OutputFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpg => "jpg",
            Self::Png => "png",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RendererMode {
    Image,
    Shader,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShaderDesktopScope {
    Virtual,
    Primary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShaderColorSpace {
    Unorm,
    Srgb,
}

#[derive(Debug, Clone, Deserialize)]
struct RawShaderConfig {
    name: Option<Lenient<String>>,
    target_fps: Option<Lenient<u16>>,
    resolution: Option<Lenient<u8>>,
    mouse_enabled: Option<Lenient<bool>>,
    desktop_scope: Option<Lenient<ShaderDesktopScope>>,
    color_space: Option<Lenient<ShaderColorSpace>>,
    #[serde(flatten)]
    extra: hcl::Map<String, hcl::Value>,
}

fn default_recursive() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum RawSourceConfig {
    File {
        path: PathBuf,
        #[serde(flatten)]
        extra: hcl::Map<String, hcl::Value>,
    },
    Directory {
        path: PathBuf,
        #[serde(default = "default_recursive")]
        recursive: bool,
        extensions: Option<Vec<String>>,
        #[serde(flatten)]
        extra: hcl::Map<String, hcl::Value>,
    },
    Rss {
        url: String,
        max_items: Option<usize>,
        download_dir: Option<PathBuf>,
        #[serde(flatten)]
        extra: hcl::Map<String, hcl::Value>,
    },
    Wallhaven {
        query: Option<String>,
        categories: Option<String>,
        purity: Option<String>,
        sorting: Option<String>,
        #[serde(rename = "topRange")]
        top_range: Option<String>,
        atleast: Option<String>,
        #[serde(rename = "maxItems")]
        max_items: Option<usize>,
        #[serde(rename = "apiKey")]
        api_key: Option<String>,
        #[serde(flatten)]
        extra: hcl::Map<String, hcl::Value>,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum DurationInput {
    Seconds(u64),
    Text(String),
}

#[derive(Debug, Clone, Deserialize)]
struct RawImageConfig {
    timer: Option<Lenient<DurationInput>>,
    #[serde(rename = "remoteUpdateTimer")]
    remote_update_timer: Option<Lenient<DurationInput>>,
    sources: Option<Lenient<Vec<Lenient<RawSourceConfig>>>>,
    format: Option<Lenient<OutputFormat>>,
    jpeg_quality: Option<Lenient<u8>>,
    #[serde(flatten)]
    extra: hcl::Map<String, hcl::Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawUpdaterConfig {
    enabled: Option<Lenient<bool>>,
    #[serde(rename = "checkInterval")]
    check_interval: Option<Lenient<DurationInput>>,
    #[serde(rename = "feedUrl")]
    feed_url: Option<Lenient<String>>,
    #[serde(flatten)]
    extra: hcl::Map<String, hcl::Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawConfig {
    image: Option<Lenient<RawImageConfig>>,
    updater: Option<Lenient<RawUpdaterConfig>>,
    cache_dir: Option<Lenient<PathBuf>>,
    state_file: Option<Lenient<PathBuf>>,
    log_level: Option<Lenient<String>>,
    max_cache_mb: Option<Lenient<u64>>,
    max_cache_age_days: Option<Lenient<u64>>,
    renderer: Option<Lenient<RendererMode>>,
    shader: Option<Lenient<RawShaderConfig>>,
    proxy: Option<Lenient<String>>,
    #[serde(flatten)]
    extra: hcl::Map<String, hcl::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Lenient<T> {
    Valid(T),
    Invalid(hcl::Value),
}

#[derive(Debug, Clone)]
pub enum SourceConfig {
    File {
        path: PathBuf,
    },
    Directory {
        path: PathBuf,
        recursive: bool,
        extensions: Option<Vec<String>>,
    },
    Rss {
        url: String,
        max_items: usize,
        download_dir: Option<PathBuf>,
    },
    Wallhaven {
        query: Option<String>,
        categories: Option<String>,
        purity: Option<String>,
        sorting: Option<String>,
        top_range: Option<String>,
        atleast: Option<String>,
        max_items: usize,
        api_key: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct AuraConfig {
    pub image: ImageConfig,
    pub updater: UpdaterConfig,
    pub cache_dir: PathBuf,
    pub state_file: PathBuf,
    pub log_level: String,
    pub max_cache_bytes: u64,
    pub max_cache_age: Duration,
    pub renderer: RendererMode,
    pub shader: Option<ShaderConfig>,
    /// Optional HTTP(S) proxy used by every remote source (RSS, Wallhaven).
    ///
    /// Remote sources are unreachable on networks where those hosts are
    /// blocked, so a local proxy can be configured explicitly instead of
    /// relying on the `HTTPS_PROXY` environment variables alone.
    pub proxy: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ImageConfig {
    pub timer: Duration,
    pub remote_update_timer: Duration,
    pub sources: Vec<SourceConfig>,
    pub format: OutputFormat,
    pub jpeg_quality: u8,
}

#[derive(Debug, Clone)]
pub struct UpdaterConfig {
    pub enabled: bool,
    pub check_interval: Duration,
    pub feed_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShaderConfig {
    pub name: String,
    pub target_fps: u16,
    pub resolution: u8,
    pub mouse_enabled: bool,
    pub desktop_scope: ShaderDesktopScope,
    pub color_space: ShaderColorSpace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWarning {
    pub key_path: String,
    pub issue: String,
    pub fallback: String,
    pub raw_value: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ConfigWithWarnings {
    pub config: AuraConfig,
    pub warnings: Vec<ConfigWarning>,
}

impl ConfigWarning {
    fn unknown_key(key_path: String, raw_value: &hcl::Value) -> Self {
        Self {
            key_path,
            issue: "unknown key".to_string(),
            fallback: "ignored".to_string(),
            raw_value: Some(raw_value.to_string()),
        }
    }

    fn invalid_value(
        key_path: impl Into<String>,
        issue: impl Into<String>,
        fallback: impl Into<String>,
        raw_value: Option<&hcl::Value>,
    ) -> Self {
        Self {
            key_path: key_path.into(),
            issue: issue.into(),
            fallback: fallback.into(),
            raw_value: raw_value.map(ToString::to_string),
        }
    }
}

pub fn load_from_path_with_warnings(path: &Path) -> Result<ConfigWithWarnings> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read config from {}", path.display()))?;
    parse_from_str_with_warnings(&content, path)
}

#[cfg(test)]
pub(crate) fn parse_from_str(content: &str, path: &Path) -> Result<AuraConfig> {
    Ok(parse_from_str_with_warnings(content, path)?.config)
}

pub fn parse_from_str_with_warnings(content: &str, path: &Path) -> Result<ConfigWithWarnings> {
    let raw: RawConfig =
        hcl::from_str(content).with_context(|| format!("invalid HCL in {}", path.display()))?;
    AuraConfig::from_raw(raw, path)
}

pub fn default_hcl(pictures_dir: &Path) -> String {
    let pictures = hcl_path(pictures_dir);
    format!(
        r#"# aura (Wallpaper Manager) configuration file

# Runtime renderer mode: "image" | "shader"
renderer = "image"

# Image mode options (used when renderer = "image")
image = {{
	# Image sources. Multiple sources will be combined together to pick the next wallpaper from.
	# Supported source types: "file" | "directory" | "rss" | "wallhaven"
	sources = [
        # RSS feed of ambient-tv images (~120 high-quality images)
        {{ type = "rss", url = "https://mrrtt.me/atv" }}

        # Wallhaven search results (https://wallhaven.cc)
        #{{ type = "wallhaven", query = "aurora", categories = "general", purity = "sfw", sorting = "toplist", topRange = "1M", atleast = "2560x1440", maxItems = 24 }}

        # Your own directory of images
        #{{ type = "directory", path = "{}" }}
    ]
    timer = "3h"                     # "40s" | "12m" | "3h"
	remoteUpdateTimer = "2h"         # "40s" | "12m" | "3h"
	format = "jpg"                   # "jpg" | "png"
	jpeg_quality = 90                # 1-100
}}

# Shader mode options (used when renderer = "shader")
shader = {{
	name = "gradient_glossy"         # "gradient_glossy" | "limestone_cave" | "dither_asci_1" | "dither_asci_2" | "dither_warp" | "silk"
	target_fps = 60                  # 1-999
	resolution = 100                 # 1-100 (% internal shader render resolution; output stays full-screen)
	mouse_enabled = false            # false | true
	desktop_scope = "virtual"        # "virtual" | "primary"
	color_space = "unorm"            # "unorm" | "srgb"
}}

# App update settings
updater = {{
    enabled = true
    checkInterval = "6h"
    feedUrl = "https://github.com/hmerritt/aura/releases/latest/download"
}}

# Optional HTTP(S) proxy shared by every remote source (RSS / Wallhaven).
# Remote hosts can be unreachable without one; falls back to the
# HTTPS_PROXY / HTTP_PROXY environment variables when unset.
#proxy = "http://127.0.0.1:10809"
"#,
        pictures
    )
}

impl AuraConfig {
    fn from_raw(raw: RawConfig, config_path: &Path) -> Result<ConfigWithWarnings> {
        let mut warnings = Vec::new();
        warn_unknown_keys("", &raw.extra, &mut warnings);

        let config_parent = config_path.parent().unwrap_or_else(|| Path::new("."));
        let app_dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("aura");

        let cache_path = take_lenient(
            "cache_dir",
            raw.cache_dir,
            &mut warnings,
            "using default cache directory",
        )
        .unwrap_or_else(|| app_dir.join("cache"));
        let cache_dir = resolve_path(cache_path, config_parent);
        let state_path = take_lenient(
            "state_file",
            raw.state_file,
            &mut warnings,
            "using default state file path",
        )
        .unwrap_or_else(|| app_dir.join("state.json"));
        let state_file = resolve_path(state_path, config_parent);
        let image = parse_image_config(
            take_lenient(
                "image",
                raw.image,
                &mut warnings,
                "using default image settings",
            ),
            config_parent,
            &mut warnings,
        )?;
        let updater = parse_updater_config(
            take_lenient(
                "updater",
                raw.updater,
                &mut warnings,
                "using default updater settings",
            ),
            &mut warnings,
        )?;

        let max_cache_mb = take_lenient(
            "max_cache_mb",
            raw.max_cache_mb,
            &mut warnings,
            "using default max cache size",
        )
        .unwrap_or(DEFAULT_MAX_CACHE_MB);
        let max_cache_bytes = max_cache_mb * 1024 * 1024;

        let max_cache_age_days = take_lenient(
            "max_cache_age_days",
            raw.max_cache_age_days,
            &mut warnings,
            "using default max cache age",
        )
        .unwrap_or(DEFAULT_MAX_CACHE_AGE_DAYS);
        let max_cache_age = Duration::from_secs(max_cache_age_days * 24 * 60 * 60);
        let renderer = take_lenient(
            "renderer",
            raw.renderer,
            &mut warnings,
            "using default renderer mode",
        )
        .unwrap_or(RendererMode::Image);
        let shader = parse_shader_config(
            take_lenient(
                "shader",
                raw.shader,
                &mut warnings,
                "using default shader settings",
            ),
            renderer,
            &mut warnings,
        )?;

        let log_level = take_lenient(
            "log_level",
            raw.log_level,
            &mut warnings,
            "using default log level",
        )
        .unwrap_or_else(|| "info".to_string());

        let proxy = take_lenient(
            "proxy",
            raw.proxy,
            &mut warnings,
            "remote sources will fall back to the HTTPS_PROXY environment variables",
        )
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

        Ok(ConfigWithWarnings {
            config: Self {
                image,
                updater,
                cache_dir,
                state_file,
                log_level,
                max_cache_bytes,
                max_cache_age,
                renderer,
                shader,
                proxy,
            },
            warnings,
        })
    }
}

fn parse_image_config(
    raw: Option<RawImageConfig>,
    config_parent: &Path,
    warnings: &mut Vec<ConfigWarning>,
) -> Result<ImageConfig> {
    let raw = raw.unwrap_or_else(|| RawImageConfig {
        timer: None,
        remote_update_timer: None,
        sources: None,
        format: None,
        jpeg_quality: None,
        extra: hcl::Map::new(),
    });
    warn_unknown_keys("image", &raw.extra, warnings);

    let mut timer_secs =
        parse_duration_field_with_warnings("image.timer", raw.timer, DEFAULT_TIMER_SECS, warnings);
    if timer_secs < MIN_TIMER_SECS {
        warnings.push(ConfigWarning::invalid_value(
            "image.timer",
            format!("must be at least {MIN_TIMER_SECS} seconds"),
            format!("using default {DEFAULT_TIMER_SECS}s"),
            None,
        ));
        timer_secs = DEFAULT_TIMER_SECS;
    }

    let mut remote_secs = parse_duration_field_with_warnings(
        "image.remoteUpdateTimer",
        raw.remote_update_timer,
        DEFAULT_REMOTE_UPDATE_TIMER_SECS,
        warnings,
    );
    if remote_secs < MIN_REMOTE_UPDATE_SECS {
        warnings.push(ConfigWarning::invalid_value(
            "image.remoteUpdateTimer",
            format!("must be at least {MIN_REMOTE_UPDATE_SECS} seconds"),
            format!("using default {DEFAULT_REMOTE_UPDATE_TIMER_SECS}s"),
            None,
        ));
        remote_secs = DEFAULT_REMOTE_UPDATE_TIMER_SECS;
    }

    let mut jpeg_quality = take_lenient(
        "image.jpeg_quality",
        raw.jpeg_quality,
        warnings,
        "using default jpeg quality",
    )
    .unwrap_or(DEFAULT_JPEG_QUALITY);
    if jpeg_quality == 0 || jpeg_quality > 100 {
        warnings.push(ConfigWarning::invalid_value(
            "image.jpeg_quality",
            format!("must be between 1 and 100, got {jpeg_quality}"),
            format!("using default {DEFAULT_JPEG_QUALITY}"),
            None,
        ));
        jpeg_quality = DEFAULT_JPEG_QUALITY;
    }

    let mut sources = match take_lenient(
        "image.sources",
        raw.sources,
        warnings,
        "using default source list",
    ) {
        Some(raw_sources) => parse_sources(raw_sources, config_parent, warnings),
        None => default_sources(config_parent, warnings),
    };

    if sources.is_empty() {
        warnings.push(ConfigWarning::invalid_value(
            "image.sources",
            "no valid sources found",
            "using default source list",
            None,
        ));
        sources = default_sources(config_parent, warnings);
    }

    let format = take_lenient(
        "image.format",
        raw.format,
        warnings,
        "using default image format",
    )
    .unwrap_or(OutputFormat::Jpg);

    Ok(ImageConfig {
        timer: Duration::from_secs(timer_secs),
        remote_update_timer: Duration::from_secs(remote_secs),
        sources,
        format,
        jpeg_quality,
    })
}

fn parse_updater_config(
    raw: Option<RawUpdaterConfig>,
    warnings: &mut Vec<ConfigWarning>,
) -> Result<UpdaterConfig> {
    let raw = raw.unwrap_or_else(|| RawUpdaterConfig {
        enabled: None,
        check_interval: None,
        feed_url: None,
        extra: hcl::Map::new(),
    });
    warn_unknown_keys("updater", &raw.extra, warnings);

    let mut check_interval_secs = parse_duration_field_with_warnings(
        "updater.checkInterval",
        raw.check_interval,
        DEFAULT_UPDATER_CHECK_INTERVAL_SECS,
        warnings,
    );
    if check_interval_secs < MIN_UPDATER_CHECK_INTERVAL_SECS {
        warnings.push(ConfigWarning::invalid_value(
            "updater.checkInterval",
            format!("must be at least {MIN_UPDATER_CHECK_INTERVAL_SECS} seconds"),
            format!("using default {DEFAULT_UPDATER_CHECK_INTERVAL_SECS}s"),
            None,
        ));
        check_interval_secs = DEFAULT_UPDATER_CHECK_INTERVAL_SECS;
    }

    let feed_url_input = take_lenient(
        "updater.feedUrl",
        raw.feed_url,
        warnings,
        "using default updater feed URL",
    )
    .unwrap_or_else(|| DEFAULT_UPDATER_FEED_URL.to_string());
    let feed_url = match normalize_updater_feed_url(&feed_url_input) {
        Ok(feed_url) => feed_url,
        Err(error) => {
            warnings.push(ConfigWarning::invalid_value(
                "updater.feedUrl",
                error.to_string(),
                format!("using default {DEFAULT_UPDATER_FEED_URL}"),
                None,
            ));
            DEFAULT_UPDATER_FEED_URL.to_string()
        }
    };

    let enabled = take_lenient(
        "updater.enabled",
        raw.enabled,
        warnings,
        "using updater default",
    )
    .unwrap_or(true);

    Ok(UpdaterConfig {
        enabled,
        check_interval: Duration::from_secs(check_interval_secs),
        feed_url,
    })
}

fn default_raw_image_sources(config_parent: &Path) -> Vec<RawSourceConfig> {
    let fallback_path = if let Some(path) = dirs::picture_dir() {
        if path.exists() && path.is_dir() {
            path
        } else if let Some(home) = dirs::home_dir() {
            let home_pictures = home.join("Pictures");
            if home_pictures.exists() && home_pictures.is_dir() {
                home_pictures
            } else {
                config_parent.to_path_buf()
            }
        } else {
            config_parent.to_path_buf()
        }
    } else if let Some(home) = dirs::home_dir() {
        let home_pictures = home.join("Pictures");
        if home_pictures.exists() && home_pictures.is_dir() {
            home_pictures
        } else {
            config_parent.to_path_buf()
        }
    } else {
        config_parent.to_path_buf()
    };

    vec![RawSourceConfig::Directory {
        path: fallback_path,
        recursive: true,
        extensions: Some(
            ["jpg", "jpeg", "png", "webp", "bmp", "gif"]
                .into_iter()
                .map(|x| x.to_string())
                .collect(),
        ),
        extra: hcl::Map::new(),
    }]
}

fn parse_shader_config(
    raw: Option<RawShaderConfig>,
    renderer: RendererMode,
    warnings: &mut Vec<ConfigWarning>,
) -> Result<Option<ShaderConfig>> {
    let Some(raw) = raw else {
        if renderer == RendererMode::Shader {
            return Ok(Some(default_shader_config()));
        }
        return Ok(None);
    };
    warn_unknown_keys("shader", &raw.extra, warnings);

    let mut target_fps = take_lenient(
        "shader.target_fps",
        raw.target_fps,
        warnings,
        "using default shader FPS",
    )
    .unwrap_or(DEFAULT_SHADER_TARGET_FPS);
    if target_fps == 0 || target_fps > 240 {
        warnings.push(ConfigWarning::invalid_value(
            "shader.target_fps",
            format!("must be between 1 and 240, got {target_fps}"),
            format!("using default {DEFAULT_SHADER_TARGET_FPS}"),
            None,
        ));
        target_fps = DEFAULT_SHADER_TARGET_FPS;
    }

    let mut resolution = take_lenient(
        "shader.resolution",
        raw.resolution,
        warnings,
        "using default shader resolution",
    )
    .unwrap_or(DEFAULT_SHADER_RESOLUTION_PERCENT);
    if resolution == 0 || resolution > 100 {
        warnings.push(ConfigWarning::invalid_value(
            "shader.resolution",
            format!("must be between 1 and 100, got {resolution}"),
            format!("using default {DEFAULT_SHADER_RESOLUTION_PERCENT}"),
            None,
        ));
        resolution = DEFAULT_SHADER_RESOLUTION_PERCENT;
    }

    let mut name = take_lenient(
        "shader.name",
        raw.name,
        warnings,
        "using default shader name",
    )
    .unwrap_or_else(|| DEFAULT_SHADER_NAME.to_string())
    .trim()
    .to_string();
    if name.is_empty() {
        warnings.push(ConfigWarning::invalid_value(
            "shader.name",
            "must not be empty",
            format!("using default {DEFAULT_SHADER_NAME}"),
            None,
        ));
        name = DEFAULT_SHADER_NAME.to_string();
    }
    let name = if name == LEGACY_SHADER_NAME {
        DEFAULT_SHADER_NAME.to_string()
    } else {
        name
    };

    let mouse_enabled = take_lenient(
        "shader.mouse_enabled",
        raw.mouse_enabled,
        warnings,
        "using shader mouse default",
    )
    .unwrap_or(false);
    let desktop_scope = take_lenient(
        "shader.desktop_scope",
        raw.desktop_scope,
        warnings,
        "using shader desktop scope default",
    )
    .unwrap_or(ShaderDesktopScope::Virtual);
    let color_space = take_lenient(
        "shader.color_space",
        raw.color_space,
        warnings,
        "using shader color space default",
    )
    .unwrap_or(DEFAULT_SHADER_COLOR_SPACE);

    Ok(Some(ShaderConfig {
        name,
        target_fps,
        resolution,
        mouse_enabled,
        desktop_scope,
        color_space,
    }))
}

fn default_shader_config() -> ShaderConfig {
    ShaderConfig {
        name: DEFAULT_SHADER_NAME.to_string(),
        target_fps: DEFAULT_SHADER_TARGET_FPS,
        resolution: DEFAULT_SHADER_RESOLUTION_PERCENT,
        mouse_enabled: false,
        desktop_scope: ShaderDesktopScope::Virtual,
        color_space: DEFAULT_SHADER_COLOR_SPACE,
    }
}

fn parse_sources(
    raw_sources: Vec<Lenient<RawSourceConfig>>,
    config_parent: &Path,
    warnings: &mut Vec<ConfigWarning>,
) -> Vec<SourceConfig> {
    let mut sources = Vec::new();
    for (index, raw_source) in raw_sources.into_iter().enumerate() {
        let field = format!("image.sources[{index}]");
        let Some(source) =
            take_lenient(&field, Some(raw_source), warnings, "dropping source entry")
        else {
            continue;
        };
        warn_unknown_source_keys(&field, &source, warnings);
        match validate_source(source, config_parent) {
            Ok(source) => sources.push(source),
            Err(error) => warnings.push(ConfigWarning::invalid_value(
                field,
                error.to_string(),
                "dropping source entry",
                None,
            )),
        }
    }
    sources
}

fn default_sources(config_parent: &Path, warnings: &mut Vec<ConfigWarning>) -> Vec<SourceConfig> {
    let mut sources = Vec::new();
    for source in default_raw_image_sources(config_parent) {
        match validate_source(source, config_parent) {
            Ok(source) => sources.push(source),
            Err(error) => warnings.push(ConfigWarning::invalid_value(
                "image.sources",
                error.to_string(),
                "dropping source entry",
                None,
            )),
        }
    }

    if sources.is_empty() {
        sources.push(SourceConfig::Directory {
            path: config_parent.to_path_buf(),
            recursive: true,
            extensions: Some(
                ["jpg", "jpeg", "png", "webp", "bmp", "gif"]
                    .into_iter()
                    .map(|x| x.to_string())
                    .collect(),
            ),
        });
    }
    sources
}

fn warn_unknown_source_keys(
    source_path: &str,
    source: &RawSourceConfig,
    warnings: &mut Vec<ConfigWarning>,
) {
    match source {
        RawSourceConfig::File { extra, .. }
        | RawSourceConfig::Directory { extra, .. }
        | RawSourceConfig::Rss { extra, .. }
        | RawSourceConfig::Wallhaven { extra, .. } => warn_unknown_keys(source_path, extra, warnings),
    }
}

fn warn_unknown_keys(
    prefix: &str,
    extras: &hcl::Map<String, hcl::Value>,
    warnings: &mut Vec<ConfigWarning>,
) {
    for (key, value) in extras {
        let key_path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        warnings.push(ConfigWarning::unknown_key(key_path, value));
    }
}

fn take_lenient<T>(
    key_path: &str,
    value: Option<Lenient<T>>,
    warnings: &mut Vec<ConfigWarning>,
    fallback: &str,
) -> Option<T> {
    match value {
        Some(Lenient::Valid(value)) => Some(value),
        Some(Lenient::Invalid(raw_value)) => {
            warnings.push(ConfigWarning::invalid_value(
                key_path,
                "invalid value",
                fallback,
                Some(&raw_value),
            ));
            None
        }
        None => None,
    }
}

fn parse_duration_field_with_warnings(
    field_name: &str,
    value: Option<Lenient<DurationInput>>,
    default_secs: u64,
    warnings: &mut Vec<ConfigWarning>,
) -> u64 {
    match value {
        Some(Lenient::Valid(value)) => {
            match parse_duration_field(field_name, Some(value), default_secs) {
                Ok(secs) => secs,
                Err(error) => {
                    warnings.push(ConfigWarning::invalid_value(
                        field_name,
                        error.to_string(),
                        format!("using default {default_secs}s"),
                        None,
                    ));
                    default_secs
                }
            }
        }
        Some(Lenient::Invalid(raw_value)) => {
            warnings.push(ConfigWarning::invalid_value(
                field_name,
                "invalid duration value",
                format!("using default {default_secs}s"),
                Some(&raw_value),
            ));
            default_secs
        }
        None => default_secs,
    }
}

fn parse_duration_field(
    field_name: &str,
    value: Option<DurationInput>,
    default_secs: u64,
) -> Result<u64> {
    match value {
        Some(DurationInput::Seconds(secs)) => Ok(secs),
        Some(DurationInput::Text(raw)) => parse_duration_string(field_name, &raw),
        None => Ok(default_secs),
    }
}

fn parse_duration_string(field_name: &str, raw: &str) -> Result<u64> {
    let trimmed = raw.trim();
    let Some((unit_pos, unit_char)) = trimmed
        .char_indices()
        .rev()
        .find(|(_, ch)| !ch.is_whitespace())
    else {
        bail!(
            "{field_name} must be a positive integer or duration string like \"40s\", \"12m\", \"3h\""
        );
    };

    let unit = unit_char.to_ascii_lowercase();
    let multiplier = match unit {
        's' => 1_u64,
        'm' => 60_u64,
        'h' => 3600_u64,
        _ => {
            bail!(
                "{field_name} has invalid duration unit \"{unit_char}\"; expected one of: s, m, h"
            )
        }
    };

    let number_str = trimmed[..unit_pos].trim();
    if number_str.is_empty() || !number_str.chars().all(|ch| ch.is_ascii_digit()) {
        bail!(
            "{field_name} must be a positive integer followed by s/m/h, for example \"40s\", \"12m\", \"3h\""
        );
    }

    let value = number_str.parse::<u64>().with_context(|| {
        format!("{field_name} must contain a valid integer before its unit; got \"{number_str}\"")
    })?;

    value.checked_mul(multiplier).with_context(|| {
        format!("{field_name} duration is too large to represent in seconds: \"{trimmed}\"")
    })
}

fn validate_source(source: RawSourceConfig, config_parent: &Path) -> Result<SourceConfig> {
    match source {
        RawSourceConfig::File { path, .. } => {
            let path = resolve_path(path, config_parent);
            if !path.exists() || !path.is_file() {
                bail!(
                    "file source does not exist or is not a file: {}",
                    path.display()
                );
            }
            Ok(SourceConfig::File { path })
        }
        RawSourceConfig::Directory {
            path,
            recursive,
            extensions,
            ..
        } => {
            let path = resolve_path(path, config_parent);
            if !path.exists() || !path.is_dir() {
                bail!(
                    "directory source does not exist or is not a directory: {}",
                    path.display()
                );
            }
            let extensions = extensions.map(|values| {
                values
                    .into_iter()
                    .map(|x| x.trim().trim_start_matches('.').to_ascii_lowercase())
                    .filter(|x| !x.is_empty())
                    .collect::<Vec<_>>()
            });
            Ok(SourceConfig::Directory {
                path,
                recursive,
                extensions,
            })
        }
        RawSourceConfig::Rss {
            url,
            max_items,
            download_dir,
            ..
        } => {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                bail!("rss url must start with http:// or https://: {url}");
            }
            let download_dir = download_dir.map(|p| resolve_path(p, config_parent));
            Ok(SourceConfig::Rss {
                url,
                max_items: max_items.unwrap_or(1000),
                download_dir,
            })
        }
        RawSourceConfig::Wallhaven {
            query,
            categories,
            purity,
            sorting,
            top_range,
            atleast,
            max_items,
            api_key,
            ..
        } => Ok(SourceConfig::Wallhaven {
            query,
            categories,
            purity,
            sorting,
            top_range,
            atleast,
            max_items: max_items.unwrap_or(24).clamp(1, 1000),
            api_key,
        }),
    }
}

fn resolve_path(path: PathBuf, config_parent: &Path) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        config_parent.join(path)
    }
}

fn hcl_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .replace('"', "\\\"")
}

fn normalize_updater_feed_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("updater.feedUrl must not be empty");
    }

    let parsed = url::Url::parse(trimmed)
        .with_context(|| format!("updater.feedUrl must be a valid URL: {trimmed}"))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        bail!("updater.feedUrl must use http:// or https://");
    }

    Ok(trimmed.trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn has_warning(warnings: &[ConfigWarning], key_path: &str) -> bool {
        warnings.iter().any(|warning| warning.key_path == key_path)
    }

    #[test]
    fn parses_valid_config() {
        let tmp = tempdir().unwrap();
        let img = tmp.path().join("a.jpg");
        let dir = tmp.path().join("imgs");
        fs::write(&img, b"not-an-image").unwrap();
        fs::create_dir_all(&dir).unwrap();

        let raw = format!(
            r#"
image = {{
  timer = 15
  remoteUpdateTimer = 600
  sources = [
    {{ type = "file", path = "{}" }},
    {{ type = "directory", path = "{}", recursive = false }},
    {{ type = "rss", url = "https://example.com/feed.xml", max_items = 20 }}
  ]
}}
"#,
            hcl_path(&img),
            hcl_path(&dir)
        );

        let cfg = parse_from_str(&raw, &tmp.path().join("aura.hcl")).unwrap();
        assert_eq!(cfg.image.timer.as_secs(), 15);
        assert_eq!(cfg.image.remote_update_timer.as_secs(), 600);
        assert_eq!(cfg.image.sources.len(), 3);
    }

    #[test]
    fn falls_back_for_tiny_timer() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("imgs");
        fs::create_dir_all(&dir).unwrap();

        let raw = format!(
            r#"
image = {{
  timer = 2
  sources = [ {{ type = "directory", path = "{}" }} ]
}}
"#,
            hcl_path(&dir)
        );

        let parsed = parse_from_str_with_warnings(&raw, &tmp.path().join("aura.hcl")).unwrap();
        assert_eq!(parsed.config.image.timer.as_secs(), DEFAULT_TIMER_SECS);
        assert!(has_warning(&parsed.warnings, "image.timer"));
    }

    #[test]
    fn generated_default_hcl_parses() {
        let tmp = tempdir().unwrap();
        let pictures = tmp.path().join("Pictures");
        fs::create_dir_all(&pictures).unwrap();

        let raw = default_hcl(&pictures);
        assert!(raw.contains("name = \"gradient_glossy\""));
        assert!(raw.contains("color_space = \"unorm\""));
        assert!(raw.contains("resolution = 100"));
        assert!(raw.contains("updater = {"));
        assert!(raw.contains(&format!(
            "#{{ type = \"directory\", path = \"{}\" }}",
            hcl_path(&pictures)
        )));
        let cfg = parse_from_str(&raw, &tmp.path().join("aura.hcl")).unwrap();
        // `default_hcl` uses explicit template durations (3h / 2h), not parser fallback defaults.
        assert_eq!(cfg.image.timer.as_secs(), 10_800);
        assert_eq!(cfg.image.remote_update_timer.as_secs(), 7_200);
        assert!(cfg.updater.enabled);
        assert_eq!(cfg.updater.check_interval.as_secs(), 21_600);
        assert_eq!(cfg.updater.feed_url, DEFAULT_UPDATER_FEED_URL);
        assert_eq!(cfg.image.sources.len(), 1);

        match &cfg.image.sources[0] {
            SourceConfig::Rss {
                url,
                max_items,
                download_dir,
            } => {
                assert_eq!(url, "https://mrrtt.me/atv");
                assert_eq!(*max_items, 1000);
                assert!(download_dir.is_none());
            }
            _ => panic!("expected RSS source in generated config"),
        }

        let shader = cfg.shader.expect("shader config should exist");
        assert_eq!(shader.color_space, ShaderColorSpace::Unorm);
        assert_eq!(shader.resolution, DEFAULT_SHADER_RESOLUTION_PERCENT);
    }

    #[test]
    fn deprecated_shader_fields_are_ignored() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("imgs");
        fs::create_dir_all(&dir).unwrap();

        let raw = format!(
            r#"
renderer = "shader"
image = {{
  sources = [ {{ type = "directory", path = "{}" }} ]
}}
shader = {{
  name = "gradient_glossy"
  quality = "ultra"
  memory_target_mb = 9999
  power_preference = "not_a_real_mode"
  max_frame_latency = 99
  crate_path = "shaders/legacy"
  hot_reload = true
  reload_debounce_ms = 500
  target_fps = 75
  mouse_enabled = true
}}
"#,
            hcl_path(&dir)
        );

        let cfg = parse_from_str(&raw, &tmp.path().join("aura.hcl")).unwrap();
        let shader = cfg.shader.expect("shader config should exist");
        assert_eq!(shader.name, "gradient_glossy");
        assert_eq!(shader.target_fps, 75);
        assert_eq!(shader.resolution, DEFAULT_SHADER_RESOLUTION_PERCENT);
        assert!(shader.mouse_enabled);
        assert_eq!(shader.desktop_scope, ShaderDesktopScope::Virtual);
        assert_eq!(shader.color_space, ShaderColorSpace::Unorm);

        let parsed = parse_from_str_with_warnings(&raw, &tmp.path().join("aura.hcl")).unwrap();
        assert!(has_warning(&parsed.warnings, "shader.quality"));
        assert!(has_warning(&parsed.warnings, "shader.memory_target_mb"));
        assert!(has_warning(&parsed.warnings, "shader.power_preference"));
        assert!(has_warning(&parsed.warnings, "shader.max_frame_latency"));
        assert!(has_warning(&parsed.warnings, "shader.crate_path"));
        assert!(has_warning(&parsed.warnings, "shader.hot_reload"));
        assert!(has_warning(&parsed.warnings, "shader.reload_debounce_ms"));
    }

    #[test]
    fn legacy_shader_name_alias_maps_to_gradient_glossy() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("imgs");
        fs::create_dir_all(&dir).unwrap();

        let raw = format!(
            r#"
renderer = "shader"
image = {{
  sources = [ {{ type = "directory", path = "{}" }} ]
}}
shader = {{
  name = "gradient_shader"
}}
"#,
            hcl_path(&dir)
        );

        let cfg = parse_from_str(&raw, &tmp.path().join("aura.hcl")).unwrap();
        let shader = cfg.shader.expect("shader config should exist");
        assert_eq!(shader.name, "gradient_glossy");
        assert_eq!(shader.resolution, DEFAULT_SHADER_RESOLUTION_PERCENT);
        assert_eq!(shader.desktop_scope, ShaderDesktopScope::Virtual);
        assert_eq!(shader.color_space, ShaderColorSpace::Unorm);
    }

    #[test]
    fn parses_shader_desktop_scope() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("imgs");
        fs::create_dir_all(&dir).unwrap();

        let raw = format!(
            r#"
renderer = "shader"
image = {{
  sources = [ {{ type = "directory", path = "{}" }} ]
}}
shader = {{
  name = "gradient_glossy"
  desktop_scope = "primary"
}}
"#,
            hcl_path(&dir)
        );

        let cfg = parse_from_str(&raw, &tmp.path().join("aura.hcl")).unwrap();
        let shader = cfg.shader.expect("shader config should exist");
        assert_eq!(shader.desktop_scope, ShaderDesktopScope::Primary);
        assert_eq!(shader.resolution, DEFAULT_SHADER_RESOLUTION_PERCENT);
        assert_eq!(shader.color_space, ShaderColorSpace::Unorm);
    }

    #[test]
    fn ignores_shader_quality_options() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("imgs");
        fs::create_dir_all(&dir).unwrap();

        let raw = format!(
            r#"
renderer = "shader"
image = {{
  sources = [ {{ type = "directory", path = "{}" }} ]
}}
shader = {{
  quality = "ultra"
}}
"#,
            hcl_path(&dir)
        );
        let cfg = parse_from_str(&raw, &tmp.path().join("aura.hcl")).unwrap();
        let shader = cfg.shader.expect("shader config should exist");
        assert_eq!(shader.name, "gradient_glossy");
        assert_eq!(shader.resolution, DEFAULT_SHADER_RESOLUTION_PERCENT);
        assert_eq!(shader.desktop_scope, ShaderDesktopScope::Virtual);
        assert_eq!(shader.color_space, ShaderColorSpace::Unorm);
    }

    #[test]
    fn parses_shader_color_space_options() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("imgs");
        fs::create_dir_all(&dir).unwrap();

        let raw = format!(
            r#"
renderer = "shader"
image = {{
  sources = [ {{ type = "directory", path = "{}" }} ]
}}
shader = {{
  color_space = "srgb"
}}
"#,
            hcl_path(&dir)
        );

        let cfg = parse_from_str(&raw, &tmp.path().join("aura.hcl")).unwrap();
        let shader = cfg.shader.expect("shader config should exist");
        assert_eq!(shader.color_space, ShaderColorSpace::Srgb);
        assert_eq!(shader.resolution, DEFAULT_SHADER_RESOLUTION_PERCENT);
    }

    #[test]
    fn falls_back_for_invalid_shader_color_space_options() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("imgs");
        fs::create_dir_all(&dir).unwrap();

        let raw = format!(
            r#"
renderer = "shader"
image = {{
  sources = [ {{ type = "directory", path = "{}" }} ]
}}
shader = {{
  color_space = "display_p3"
}}
"#,
            hcl_path(&dir)
        );
        let parsed = parse_from_str_with_warnings(&raw, &tmp.path().join("aura.hcl")).unwrap();
        let shader = parsed.config.shader.expect("shader config should exist");
        assert_eq!(shader.color_space, ShaderColorSpace::Unorm);
        assert_eq!(shader.resolution, DEFAULT_SHADER_RESOLUTION_PERCENT);
        assert!(has_warning(&parsed.warnings, "shader.color_space"));
    }

    #[test]
    fn parses_shader_resolution_percentage() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("imgs");
        fs::create_dir_all(&dir).unwrap();

        let raw = format!(
            r#"
renderer = "shader"
image = {{
  sources = [ {{ type = "directory", path = "{}" }} ]
}}
shader = {{
  resolution = 50
}}
"#,
            hcl_path(&dir)
        );

        let cfg = parse_from_str(&raw, &tmp.path().join("aura.hcl")).unwrap();
        let shader = cfg.shader.expect("shader config should exist");
        assert_eq!(shader.resolution, 50);
    }

    #[test]
    fn falls_back_for_invalid_shader_resolution_percentage() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("imgs");
        fs::create_dir_all(&dir).unwrap();

        for resolution in [0_u16, 101_u16] {
            let raw = format!(
                r#"
renderer = "shader"
image = {{
  sources = [ {{ type = "directory", path = "{}" }} ]
}}
shader = {{
  resolution = {}
}}
"#,
                hcl_path(&dir),
                resolution
            );

            let parsed = parse_from_str_with_warnings(&raw, &tmp.path().join("aura.hcl")).unwrap();
            let shader = parsed.config.shader.expect("shader config should exist");
            assert_eq!(shader.resolution, DEFAULT_SHADER_RESOLUTION_PERCENT);
            assert!(has_warning(&parsed.warnings, "shader.resolution"));
        }
    }

    #[test]
    fn parses_duration_strings_for_timer_fields() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("imgs");
        fs::create_dir_all(&dir).unwrap();

        let raw = format!(
            r#"
image = {{
  timer = "40s"
  remoteUpdateTimer = "12m"
  sources = [ {{ type = "directory", path = "{}" }} ]
}}
"#,
            hcl_path(&dir)
        );

        let cfg = parse_from_str(&raw, &tmp.path().join("aura.hcl")).unwrap();
        assert_eq!(cfg.image.timer.as_secs(), 40);
        assert_eq!(cfg.image.remote_update_timer.as_secs(), 720);
    }

    #[test]
    fn parses_case_insensitive_duration_strings_with_spaces() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("imgs");
        fs::create_dir_all(&dir).unwrap();

        let raw = format!(
            r#"
image = {{
  timer = "3 H"
  remoteUpdateTimer = "40 S"
  sources = [ {{ type = "directory", path = "{}" }} ]
}}
"#,
            hcl_path(&dir)
        );

        let cfg = parse_from_str(&raw, &tmp.path().join("aura.hcl")).unwrap();
        assert_eq!(cfg.image.timer.as_secs(), 10_800);
        assert_eq!(cfg.image.remote_update_timer.as_secs(), 40);
    }

    #[test]
    fn falls_back_for_invalid_duration_string_formats() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("imgs");
        fs::create_dir_all(&dir).unwrap();

        for timer in [
            "\"40\"",
            "\"1d\"",
            "\"abc\"",
            "\"-5m\"",
            "\"1.5h\"",
            "\"1h30m\"",
        ] {
            let raw = format!(
                r#"
image = {{
  timer = {}
  sources = [ {{ type = "directory", path = "{}" }} ]
}}
"#,
                timer,
                hcl_path(&dir)
            );
            let parsed = parse_from_str_with_warnings(&raw, &tmp.path().join("aura.hcl")).unwrap();
            assert_eq!(
                parsed.config.image.timer.as_secs(),
                DEFAULT_TIMER_SECS,
                "expected image.timer={} to fallback",
                timer
            );
            assert!(
                has_warning(&parsed.warnings, "image.timer"),
                "expected image.timer={} warning",
                timer
            );
        }
    }

    #[test]
    fn falls_back_for_values_below_duration_minimums() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("imgs");
        fs::create_dir_all(&dir).unwrap();

        let tiny_timer = format!(
            r#"
image = {{
  timer = "2s"
  sources = [ {{ type = "directory", path = "{}" }} ]
}}
"#,
            hcl_path(&dir)
        );
        let parsed_timer =
            parse_from_str_with_warnings(&tiny_timer, &tmp.path().join("aura.hcl")).unwrap();
        assert_eq!(
            parsed_timer.config.image.timer.as_secs(),
            DEFAULT_TIMER_SECS
        );
        assert!(has_warning(&parsed_timer.warnings, "image.timer"));

        let tiny_remote = format!(
            r#"
image = {{
  timer = "10s"
  remoteUpdateTimer = "20s"
  sources = [ {{ type = "directory", path = "{}" }} ]
}}
"#,
            hcl_path(&dir)
        );
        let parsed_remote =
            parse_from_str_with_warnings(&tiny_remote, &tmp.path().join("aura.hcl")).unwrap();
        assert_eq!(
            parsed_remote.config.image.remote_update_timer.as_secs(),
            DEFAULT_REMOTE_UPDATE_TIMER_SECS
        );
        assert!(has_warning(
            &parsed_remote.warnings,
            "image.remoteUpdateTimer"
        ));
    }

    #[test]
    fn falls_back_for_overflowing_duration_values() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("imgs");
        fs::create_dir_all(&dir).unwrap();

        let huge = format!("\"{}h\"", u64::MAX);
        let raw = format!(
            r#"
image = {{
  timer = {}
  sources = [ {{ type = "directory", path = "{}" }} ]
}}
"#,
            huge,
            hcl_path(&dir)
        );

        let parsed = parse_from_str_with_warnings(&raw, &tmp.path().join("aura.hcl")).unwrap();
        assert_eq!(parsed.config.image.timer.as_secs(), DEFAULT_TIMER_SECS);
        assert!(has_warning(&parsed.warnings, "image.timer"));
    }

    #[test]
    fn missing_image_uses_defaults() {
        let tmp = tempdir().unwrap();

        let raw = "";
        let cfg = parse_from_str(raw, &tmp.path().join("aura.hcl")).unwrap();
        assert_eq!(cfg.image.timer.as_secs(), 300);
        assert_eq!(cfg.image.remote_update_timer.as_secs(), 3600);
        assert!(cfg.updater.enabled);
        assert_eq!(cfg.updater.check_interval.as_secs(), 21_600);
        assert_eq!(cfg.updater.feed_url, DEFAULT_UPDATER_FEED_URL);
        assert_eq!(cfg.image.format, OutputFormat::Jpg);
        assert_eq!(cfg.image.jpeg_quality, 90);
        assert_eq!(cfg.image.sources.len(), 1);
    }

    #[test]
    fn parses_explicit_updater_config() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("imgs");
        fs::create_dir_all(&dir).unwrap();

        let raw = format!(
            r#"
image = {{
  sources = [ {{ type = "directory", path = "{}" }} ]
}}
updater = {{
  enabled = false
  checkInterval = "45m"
  feedUrl = "https://updates.example.com/aura/"
}}
"#,
            hcl_path(&dir)
        );

        let cfg = parse_from_str(&raw, &tmp.path().join("aura.hcl")).unwrap();
        assert!(!cfg.updater.enabled);
        assert_eq!(cfg.updater.check_interval.as_secs(), 2_700);
        assert_eq!(cfg.updater.feed_url, "https://updates.example.com/aura");
    }

    #[test]
    fn falls_back_for_invalid_updater_check_interval() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("imgs");
        fs::create_dir_all(&dir).unwrap();

        let raw = format!(
            r#"
image = {{
  sources = [ {{ type = "directory", path = "{}" }} ]
}}
updater = {{
  checkInterval = "5m"
}}
"#,
            hcl_path(&dir)
        );

        let parsed = parse_from_str_with_warnings(&raw, &tmp.path().join("aura.hcl")).unwrap();
        assert_eq!(
            parsed.config.updater.check_interval.as_secs(),
            DEFAULT_UPDATER_CHECK_INTERVAL_SECS
        );
        assert!(has_warning(&parsed.warnings, "updater.checkInterval"));
    }

    #[test]
    fn falls_back_for_invalid_updater_feed_scheme() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("imgs");
        fs::create_dir_all(&dir).unwrap();

        let raw = format!(
            r#"
image = {{
  sources = [ {{ type = "directory", path = "{}" }} ]
}}
updater = {{
  feedUrl = "ftp://updates.example.com/aura"
}}
"#,
            hcl_path(&dir)
        );

        let parsed = parse_from_str_with_warnings(&raw, &tmp.path().join("aura.hcl")).unwrap();
        assert_eq!(parsed.config.updater.feed_url, DEFAULT_UPDATER_FEED_URL);
        assert!(has_warning(&parsed.warnings, "updater.feedUrl"));
    }

    #[test]
    fn warns_for_unknown_legacy_top_level_image_keys() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("imgs");
        fs::create_dir_all(&dir).unwrap();

        let raw = format!(
            r#"
image_format = "png"
sources = [ {{ type = "directory", path = "{}" }} ]
"#,
            hcl_path(&dir)
        );

        let parsed = parse_from_str_with_warnings(&raw, &tmp.path().join("aura.hcl")).unwrap();
        assert!(has_warning(&parsed.warnings, "image_format"));
        assert!(has_warning(&parsed.warnings, "sources"));
    }

    #[test]
    fn warns_for_unknown_legacy_top_level_timer() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("imgs");
        fs::create_dir_all(&dir).unwrap();

        let raw = format!(
            r#"
timer = 15
image = {{
  sources = [ {{ type = "directory", path = "{}" }} ]
}}
"#,
            hcl_path(&dir)
        );

        let parsed = parse_from_str_with_warnings(&raw, &tmp.path().join("aura.hcl")).unwrap();
        assert!(has_warning(&parsed.warnings, "timer"));
    }
}

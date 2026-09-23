use crate::cache::CacheManager;
use crate::config::OutputFormat;
use crate::errors::Result;
use anyhow::Context;
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageFormat, ImageReader};
use std::fs;
use std::path::{Path, PathBuf};

pub fn prepare_for_output(
    input: &Path,
    cache: &CacheManager,
    format: OutputFormat,
    jpeg_quality: u8,
) -> Result<PathBuf> {
    if should_passthrough(input, format) {
        return Ok(input.to_path_buf());
    }

    let key = build_cache_key(input, format, jpeg_quality)?;
    let cached = cache.processed_path_for_key(&key, format);
    if cached.exists() {
        return Ok(cached);
    }

    let source = ImageReader::open(input)
        .with_context(|| format!("failed to open image {}", input.display()))?
        .with_guessed_format()
        .with_context(|| format!("failed to guess image format {}", input.display()))?
        .decode()
        .with_context(|| format!("failed to decode image {}", input.display()))?;

    save_output(source, &cached, format, jpeg_quality)?;
    Ok(cached)
}

fn build_cache_key(input: &Path, format: OutputFormat, jpeg_quality: u8) -> Result<String> {
    let bytes = fs::read(input).with_context(|| format!("failed to read {}", input.display()))?;
    let source_hash = blake3::hash(&bytes).to_hex();
    let key = format!("{}:{}:{}", source_hash, format.extension(), jpeg_quality);
    Ok(blake3::hash(key.as_bytes()).to_hex().to_string())
}

fn should_passthrough(path: &Path, output_format: OutputFormat) -> bool {
    path_output_format(path)
        .map(|input_format| input_format == output_format)
        .unwrap_or(false)
}

fn path_output_format(path: &Path) -> Option<OutputFormat> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => Some(OutputFormat::Jpg),
        "png" => Some(OutputFormat::Png),
        _ => None,
    }
}

fn save_output(
    image: DynamicImage,
    output_path: &Path,
    format: OutputFormat,
    jpeg_quality: u8,
) -> Result<()> {
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    match format {
        OutputFormat::Jpg => {
            let mut file = fs::File::create(output_path)
                .with_context(|| format!("failed to create {}", output_path.display()))?;
            let mut encoder = JpegEncoder::new_with_quality(&mut file, jpeg_quality);
            encoder
                .encode_image(&image)
                .with_context(|| format!("failed to write {}", output_path.display()))?;
        }
        OutputFormat::Png => {
            image
                .save_with_format(output_path, ImageFormat::Png)
                .with_context(|| format!("failed to write {}", output_path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuraConfig, ImageConfig, RendererMode, UpdaterConfig};
    use image::{ImageBuffer, Rgba};
    use std::time::Duration;
    use tempfile::tempdir;

    fn test_cache_manager(base: &Path) -> CacheManager {
        let config = AuraConfig {
            image: ImageConfig {
                timer: Duration::from_secs(300),
                remote_update_timer: Duration::from_secs(3600),
                sources: Vec::new(),
                format: OutputFormat::Jpg,
                jpeg_quality: 90,
            },
            updater: UpdaterConfig {
                enabled: false,
                check_interval: Duration::from_secs(6 * 3600),
                feed_url: "https://github.com/hmerritt/aura/releases/latest/download".to_string(),
            },
            cache_dir: base.join("cache"),
            state_file: base.join("state.json"),
            log_level: "info".to_string(),
            max_cache_bytes: 1024 * 1024,
            max_cache_age: Duration::from_secs(24 * 60 * 60),
            renderer: RendererMode::Image,
            shader: None,
            proxy: None,
            tray_double_click: crate::config::TrayDoubleClickAction::Next,
        };
        CacheManager::new(&config).unwrap()
    }

    #[test]
    fn maps_path_extensions_to_output_format() {
        assert_eq!(
            path_output_format(Path::new("photo.jpg")),
            Some(OutputFormat::Jpg)
        );
        assert_eq!(
            path_output_format(Path::new("photo.jpeg")),
            Some(OutputFormat::Jpg)
        );
        assert_eq!(
            path_output_format(Path::new("photo.JPG")),
            Some(OutputFormat::Jpg)
        );
        assert_eq!(
            path_output_format(Path::new("photo.png")),
            Some(OutputFormat::Png)
        );
        assert_eq!(path_output_format(Path::new("photo.webp")), None);
    }

    #[test]
    fn passthrough_returns_original_path_without_decode() {
        let tmp = tempdir().unwrap();
        let cache = test_cache_manager(tmp.path());
        let input = tmp.path().join("broken.jpg");
        fs::write(&input, b"not-an-image").unwrap();

        let output = prepare_for_output(&input, &cache, OutputFormat::Jpg, 90).unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn conversion_reencodes_to_requested_format() {
        let tmp = tempdir().unwrap();
        let cache = test_cache_manager(tmp.path());
        let input = tmp.path().join("source.png");

        let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(2, 2, Rgba([10, 20, 30, 255]));
        DynamicImage::ImageRgba8(img)
            .save_with_format(&input, ImageFormat::Png)
            .unwrap();

        let output = prepare_for_output(&input, &cache, OutputFormat::Jpg, 90).unwrap();
        assert_ne!(output, input);
        assert_eq!(
            output.extension().and_then(|x| x.to_str()),
            Some(OutputFormat::Jpg.extension())
        );
        assert!(output.exists());
    }

    #[test]
    fn conversion_uses_cached_output() {
        let tmp = tempdir().unwrap();
        let cache = test_cache_manager(tmp.path());
        let input = tmp.path().join("source.png");

        let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(2, 2, Rgba([90, 80, 70, 255]));
        DynamicImage::ImageRgba8(img)
            .save_with_format(&input, ImageFormat::Png)
            .unwrap();

        let first = prepare_for_output(&input, &cache, OutputFormat::Jpg, 90).unwrap();
        let second = prepare_for_output(&input, &cache, OutputFormat::Jpg, 90).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn unknown_extension_does_not_passthrough() {
        let tmp = tempdir().unwrap();
        let cache = test_cache_manager(tmp.path());
        let input = tmp.path().join("source.bin");
        fs::write(&input, b"definitely-not-an-image").unwrap();

        let result = prepare_for_output(&input, &cache, OutputFormat::Png, 90);
        assert!(result.is_err());
    }
}

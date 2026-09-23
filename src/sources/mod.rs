pub mod directory;
pub mod rss;
pub mod wallhaven;
pub mod single;

use crate::cache::CacheManager;
use crate::config::{AuraConfig, SourceConfig};
use crate::errors::Result;
use anyhow::Context;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    File,
    Directory,
    Rss,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    File,
    Directory,
    Rss,
    Wallhaven,
}

#[derive(Debug, Clone)]
pub enum ImageLocation {
    Local(PathBuf),
    Rss {
        image_url: String,
        download_dir: PathBuf,
    },
}

#[derive(Debug, Clone)]
pub struct ImageCandidate {
    pub id: String,
    pub origin: Origin,
    pub location: ImageLocation,
    pub mtime: Option<SystemTime>,
    sort_key: String,
}

impl ImageCandidate {
    pub fn local(id: String, origin: Origin, path: PathBuf, mtime: Option<SystemTime>) -> Self {
        let sort_key = path.to_string_lossy().into_owned();
        Self {
            id,
            origin,
            location: ImageLocation::Local(path),
            mtime,
            sort_key,
        }
    }

    pub fn rss(
        id: String,
        image_url: String,
        download_dir: PathBuf,
        mtime: Option<SystemTime>,
    ) -> Self {
        Self {
            id,
            origin: Origin::Rss,
            location: ImageLocation::Rss {
                image_url: image_url.clone(),
                download_dir,
            },
            mtime,
            sort_key: image_url,
        }
    }

    pub fn sort_key(&self) -> &str {
        &self.sort_key
    }

    pub fn display_source(&self) -> String {
        match &self.location {
            ImageLocation::Local(path) => path.display().to_string(),
            ImageLocation::Rss { image_url, .. } => image_url.clone(),
        }
    }

    pub fn is_prefetchable(&self) -> bool {
        matches!(self.location, ImageLocation::Rss { .. })
    }

    #[cfg(test)]
    pub fn cached_local_path(&self) -> Result<Option<PathBuf>> {
        self.local_path()
    }

    /// Synchronous local path of the candidate (cached file for RSS, the file
    /// itself for local sources). Used to match a deleted file back to the
    /// rotation pool entry.
    pub fn local_path(&self) -> Result<Option<PathBuf>> {
        match &self.location {
            ImageLocation::Local(path) => Ok(Some(path.clone())),
            ImageLocation::Rss {
                image_url,
                download_dir,
            } => rss::find_cached_image_path(download_dir, image_url),
        }
    }

    pub async fn resolve_local_path(&self) -> Result<Option<PathBuf>> {
        match &self.location {
            ImageLocation::Local(path) => Ok(Some(path.clone())),
            ImageLocation::Rss {
                image_url,
                download_dir,
            } => rss::resolve_image_path(download_dir, image_url).await,
        }
    }

    pub async fn prefetch(&self) -> Result<()> {
        if let ImageLocation::Rss {
            image_url,
            download_dir,
        } = &self.location
        {
            let _ = rss::resolve_image_path(download_dir, image_url).await?;
        }
        Ok(())
    }
}

#[async_trait]
pub trait ImageSource: Send {
    fn name(&self) -> &str;
    fn kind(&self) -> SourceKind;
    async fn refresh(&mut self) -> Result<Vec<ImageCandidate>>;
}

pub fn build_sources(
    config: &AuraConfig,
    cache: Arc<CacheManager>,
) -> Result<Vec<Box<dyn ImageSource>>> {
    let mut sources: Vec<Box<dyn ImageSource>> = Vec::new();

    for source in &config.image.sources {
        match source {
            SourceConfig::File { path } => {
                sources.push(Box::new(single::SingleSource::new(path.clone())));
            }
            SourceConfig::Directory {
                path,
                recursive,
                extensions,
            } => {
                sources.push(Box::new(directory::DirectorySource::new(
                    path.clone(),
                    *recursive,
                    extensions.clone().unwrap_or_default(),
                )));
            }
            SourceConfig::Rss {
                url,
                max_items,
                download_dir,
            } => {
                let dir = if let Some(download_dir) = download_dir {
                    download_dir.clone()
                } else {
                    cache
                        .ensure_remote_source_dir(url)
                        .with_context(|| format!("failed to initialize RSS cache for {url}"))?
                };
                sources.push(Box::new(rss::RssSource::new(url.clone(), *max_items, dir)?));
            }
            SourceConfig::Wallhaven { .. } => {
                let dir = cache
                    .ensure_remote_source_dir("wallhaven")
                    .with_context(|| "failed to initialize Wallhaven cache")?;
                sources.push(Box::new(wallhaven::WallhavenSource::new(source, dir)?));
            }
        }
    }

    Ok(sources)
}

pub fn is_supported_image(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "jpg" | "jpeg" | "png" | "bmp" | "gif" | "webp"
            )
        })
        .unwrap_or(false)
}

pub fn image_id(prefix: &str, path: &Path) -> String {
    let raw = format!("{prefix}:{}", path.to_string_lossy());
    blake3::hash(raw.as_bytes()).to_hex().to_string()
}

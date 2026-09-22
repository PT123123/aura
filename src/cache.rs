use crate::config::{AuraConfig, OutputFormat};
use crate::errors::Result;
use anyhow::Context;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use walkdir::WalkDir;

#[derive(Debug, Clone)]
pub struct CacheManager {
    processed_dir: PathBuf,
    remote_dir: PathBuf,
    max_bytes: u64,
    max_age: Duration,
}

#[derive(Debug)]
struct CacheFile {
    path: PathBuf,
    modified: SystemTime,
    len: u64,
}

impl CacheManager {
    pub fn new(config: &AuraConfig) -> Result<Self> {
        let processed_dir = config.cache_dir.join("processed");
        let remote_dir = config.cache_dir.join("remote");

        fs::create_dir_all(&processed_dir)
            .with_context(|| format!("failed to create {}", processed_dir.display()))?;
        fs::create_dir_all(&remote_dir)
            .with_context(|| format!("failed to create {}", remote_dir.display()))?;

        Ok(Self {
            processed_dir,
            remote_dir,
            max_bytes: config.max_cache_bytes,
            max_age: config.max_cache_age,
        })
    }

    pub fn processed_path_for_key(&self, key: &str, format: OutputFormat) -> PathBuf {
        self.processed_dir
            .join(format!("{}.{}", key, format.extension()))
    }

    pub fn ensure_remote_source_dir(&self, source_hint: &str) -> Result<PathBuf> {
        let hash = blake3::hash(source_hint.as_bytes()).to_hex().to_string();
        let dir = self.remote_dir.join(hash);
        fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
        Ok(dir)
    }

    /// All image files currently present in the remote cache, recursively.
    pub fn list_remote_images(&self) -> Result<Vec<PathBuf>> {
        Ok(collect_files(&self.remote_dir)?
            .into_iter()
            .map(|file| file.path)
            .filter(|path| is_supported_image(path))
            .collect())
    }

    pub fn cleanup(&self) -> Result<()> {
        let now = SystemTime::now();
        let stale_cutoff = now
            .checked_sub(self.max_age)
            .unwrap_or(SystemTime::UNIX_EPOCH);

        let mut files = collect_files(&self.processed_dir)?;
        files.extend(collect_files(&self.remote_dir)?);

        for file in &files {
            if file.modified < stale_cutoff {
                let _ = fs::remove_file(&file.path);
            }
        }

        let mut files = collect_files(&self.processed_dir)?;
        files.extend(collect_files(&self.remote_dir)?);
        let mut total: u64 = files.iter().map(|f| f.len).sum();

        if total <= self.max_bytes {
            return Ok(());
        }

        files.sort_by_key(|f| f.modified);
        for file in files {
            if total <= self.max_bytes {
                break;
            }
            if fs::remove_file(&file.path).is_ok() {
                total = total.saturating_sub(file.len);
            }
        }
        Ok(())
    }
}

fn collect_files(root: &Path) -> Result<Vec<CacheFile>> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }

    for entry in WalkDir::new(root).into_iter().flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let metadata = fs::metadata(path)
            .with_context(|| format!("failed to read metadata for {}", path.display()))?;
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        out.push(CacheFile {
            path: path.to_path_buf(),
            modified,
            len: metadata.len(),
        });
    }
    Ok(out)
}
fn is_supported_image(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|value| value.to_str()),
        Some(ext) if matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png" | "webp" | "bmp" | "gif")
    )
}
//! Wallhaven image source.
//!
//! Pulls wallpaper candidates from the Wallhaven API
//! (`https://wallhaven.cc/api/v1/search`). Image downloads reuse the RSS
//! download/cache pipeline, so candidates are produced with `Origin::Rss`
//! semantics (remote, prefetchable, hashed cache files).

use crate::config::SourceConfig;
use crate::errors::Result;
use crate::sources::{image_id, ImageCandidate, ImageSource, SourceKind};
use crate::sources::rss::find_cached_image_path;
use anyhow::Context;
use async_trait::async_trait;
use reqwest::Client;
use std::fs;
use std::path::PathBuf;
use url::Url;

const DEFAULT_API_BASE: &str = "https://wallhaven.cc/api/v1";

/// Wallhaven encodes `categories` and `purity` as three-bit flag strings
/// (`1` = include) while the config file and the settings window write human
/// names such as `"general,anime"` or `"sfw"`.
///
/// The API silently ignores a value it cannot parse, which would quietly widen
/// the search to every category or purity instead of failing, so both shapes
/// are normalised into the flag string it actually honours.
fn normalize_flag(raw: &str, known: [&str; 3], default_all: bool) -> String {
    let trimmed = raw.trim();
    if trimmed.len() == 3 && trimmed.chars().all(|flag| flag == '0' || flag == '1') {
        return trimmed.to_string();
    }

    let lowered = trimmed.to_ascii_lowercase();
    let mut flags = [false; 3];
    let mut matched = false;
    for part in lowered.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some(index) = known.iter().position(|name| *name == part) {
            flags[index] = true;
            matched = true;
        }
    }

    if !matched {
        // An empty or unrecognised value means "no restriction" for categories
        // but "safe for work" for purity.
        return if default_all { "111" } else { "100" }.to_string();
    }

    flags
        .iter()
        .map(|flag| if *flag { '1' } else { '0' })
        .collect()
}

fn normalize_categories(raw: &str) -> String {
    normalize_flag(raw, ["general", "people", "anime"], true)
}

fn normalize_purity(raw: &str) -> String {
    normalize_flag(raw, ["sfw", "sketchy", "nsfw"], false)
}

#[derive(Debug, Clone)]
pub struct WallhavenSource {
    query: Option<String>,
    categories: String,
    purity: String,
    sorting: String,
    top_range: String,
    atleast: Option<String>,
    max_items: usize,
    api_key: Option<String>,
    download_dir: PathBuf,
    api_base: String,
    client: Client,
}

impl WallhavenSource {
    pub fn new(source: &SourceConfig, download_dir: PathBuf) -> Result<Self> {
        let (query, categories, purity, sorting, top_range, atleast, max_items, api_key) =
            match source {
                SourceConfig::Wallhaven {
                    query,
                    categories,
                    purity,
                    sorting,
                    top_range,
                    atleast,
                    max_items,
                    api_key,
                } => (
                    query.clone(),
                    categories.clone().unwrap_or_else(|| {
                        "general,people,anime".to_string()
                    }),
                    purity.clone().unwrap_or_else(|| "sfw".to_string()),
                    sorting.clone().unwrap_or_else(|| "date_added".to_string()),
                    top_range.clone().unwrap_or_else(|| "1M".to_string()),
                    atleast.clone(),
                    *max_items,
                    api_key.clone(),
                ),
                _ => anyhow::bail!("not a wallhaven source"),
            };

        fs::create_dir_all(&download_dir)
            .with_context(|| format!("failed to create {}", download_dir.display()))?;

        Ok(Self {
            query,
            categories,
            purity,
            sorting,
            top_range,
            atleast,
            max_items,
            api_key,
            download_dir,
            api_base: DEFAULT_API_BASE.to_string(),
            client: super::rss::shared_client().clone(),
        })
    }

    #[cfg(test)]
    fn with_api_base(mut self, api_base: &str) -> Self {
        self.api_base = api_base.to_string();
        self
    }

    fn search_url(&self, page: u32) -> Result<Url> {
        let mut url = Url::parse(&format!("{}/search", self.api_base))
            .with_context(|| format!("invalid wallhaven api base {}", self.api_base))?;
        {
            let mut query = url.query_pairs_mut();
            if let Some(q) = &self.query {
                if !q.trim().is_empty() {
                    query.append_pair("q", q.trim());
                }
            }
            query.append_pair("categories", &normalize_categories(&self.categories));
            query.append_pair("purity", &normalize_purity(&self.purity));
            query.append_pair("sorting", &self.sorting);
            if self.sorting == "toplist" && !self.top_range.is_empty() {
                query.append_pair("topRange", &self.top_range);
            }
            if let Some(atleast) = &self.atleast {
                if !atleast.is_empty() {
                    query.append_pair("atleast", atleast);
                }
            }
            if let Some(key) = &self.api_key {
                if !key.trim().is_empty() {
                    query.append_pair("apikey", key.trim());
                }
            }
            query.append_pair("page", &page.to_string());
        }
        Ok(url)
    }

    async fn fetch_page(&self, page: u32) -> Result<(Vec<String>, u32)> {
        let url = self.search_url(page)?;
        let body = self
            .client
            .get(url)
            .send()
            .await
            .with_context(|| format!("wallhaven request failed (page {page})"))?
            .error_for_status()
            .with_context(|| format!("wallhaven returned non-success (page {page})"))?
            .bytes()
            .await
            .with_context(|| format!("failed reading wallhaven response (page {page})"))?;

        let json: serde_json::Value = serde_json::from_slice(&body)
            .with_context(|| format!("failed to parse wallhaven response (page {page})"))?;

        let data = json
            .get("data")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        let last_page = json
            .get("meta")
            .and_then(|meta| meta.get("last_page"))
            .and_then(|value| value.as_u64())
            .unwrap_or(page as u64)
            .clamp(1, 10_000) as u32;

        let mut urls = Vec::with_capacity(data.len());
        for item in data {
            if let Some(path) = item.get("path").and_then(|value| value.as_str()) {
                if !path.trim().is_empty() {
                    urls.push(path.to_string());
                }
            }
        }
        Ok((urls, last_page))
    }
}

#[async_trait]
impl ImageSource for WallhavenSource {
    fn name(&self) -> &str {
        "wallhaven"
    }

    fn kind(&self) -> SourceKind {
        SourceKind::Wallhaven
    }

    async fn refresh(&mut self) -> Result<Vec<ImageCandidate>> {
        let mut candidates = Vec::new();
        let mut page: u32 = 1;

        loop {
            let (urls, last_page) = self.fetch_page(page).await?;
            for image_url in urls {
                let mtime = find_cached_image_path(&self.download_dir, &image_url)?
                    .and_then(|path| fs::metadata(&path).ok())
                    .and_then(|meta| meta.modified().ok());
                candidates.push(ImageCandidate::rss(
                    image_id("wallhaven", &PathBuf::from(&image_url)),
                    image_url,
                    self.download_dir.clone(),
                    mtime,
                ));
                if candidates.len() >= self.max_items {
                    break;
                }
            }

            if candidates.len() >= self.max_items || page >= last_page {
                break;
            }
            page += 1;
        }

        Ok(candidates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::ImageSource;
    use crate::sources::rss::test_support::{ResponseSpec, TestServer};

    /// Human-readable config values must reach the API as bit flags, otherwise
    /// Wallhaven ignores them and silently returns every category.
    #[test]
    fn normalises_human_readable_category_and_purity_values() {
        assert_eq!(normalize_categories("general,people,anime"), "111");
        assert_eq!(normalize_categories("general,anime"), "101");
        assert_eq!(normalize_categories("people"), "010");
        assert_eq!(
            normalize_categories("unknown-name"),
            "111",
            "unknown names fall back to all"
        );
        assert_eq!(normalize_categories(""), "111");
        assert_eq!(normalize_categories("111"), "111");
        assert_eq!(normalize_categories("001"), "001");

        assert_eq!(normalize_purity("sfw"), "100");
        assert_eq!(normalize_purity("sfw,sketchy"), "110");
        assert_eq!(normalize_purity("sketchy,nsfw"), "011");
        assert_eq!(normalize_purity(""), "100", "an empty purity stays safe for work");
        assert_eq!(normalize_purity("110"), "110");
    }

    /// The search URL must carry the normalised flags.
    #[test]
    fn search_url_uses_normalised_flags() {
        let source = WallhavenSource::new(&wallhaven_config(), PathBuf::from("wh")).unwrap();
        let url = source.search_url(3).unwrap().to_string();

        assert!(url.contains("categories=111"), "url was {url}");
        assert!(url.contains("purity=100"), "url was {url}");
        assert!(url.contains("page=3"), "url was {url}");
    }

    fn wallhaven_config() -> SourceConfig {
        SourceConfig::Wallhaven {
            query: Some("aurora".to_string()),
            categories: None,
            purity: None,
            sorting: Some("toplist".to_string()),
            top_range: Some("1M".to_string()),
            atleast: Some("1920x1080".to_string()),
            max_items: 5,
            api_key: None,
        }
    }

    fn json_page(total: usize) -> String {
        let mut items = Vec::new();
        for index in 0..total {
            items.push(format!(
                r#"{{"id":"w{index}","path":"https://w.wallhaven.cc/full/ab/w{index}.jpg"}}"#
            ));
        }
        format!(
            r#"{{"data":[{}],"meta":{{"last_page":1}}}}"#,
            items.join(",")
        )
    }

    #[tokio::test]
    async fn refresh_fetches_candidates_from_api_and_does_not_download_images() {
        let server = TestServer::start();
        let tmp = tempfile::tempdir().unwrap();
        let source = WallhavenSource::new(&wallhaven_config(), tmp.path().join("wh"))
            .unwrap()
            .with_api_base(&server.url(""));
        let search_path = {
            let url = source.search_url(1).unwrap();
            let mut full = url.path().to_string();
            if let Some(query) = url.query() {
                full.push('?');
                full.push_str(query);
            }
            full
        };
        server.set_response(
            &search_path,
            ResponseSpec::ok("application/json", json_page(4).into_bytes()),
        );

        let mut source = source;
        let candidates = source.refresh().await.unwrap();

        assert_eq!(candidates.len(), 4);
        assert!(server.hits(&search_path) >= 1);
        for candidate in &candidates {
            assert_eq!(candidate.origin, crate::sources::Origin::Rss);
            assert!(!candidate.id.is_empty());
        }
    }

    #[tokio::test]
    async fn refresh_honours_max_items() {
        let server = TestServer::start();
        let tmp = tempfile::tempdir().unwrap();
        let source = WallhavenSource::new(&wallhaven_config(), tmp.path().join("wh"))
            .unwrap()
            .with_api_base(&server.url(""));
        let search_path = {
            let url = source.search_url(1).unwrap();
            let mut full = url.path().to_string();
            if let Some(query) = url.query() {
                full.push('?');
                full.push_str(query);
            }
            full
        };
        server.set_response(
            &search_path,
            ResponseSpec::ok("application/json", json_page(24).into_bytes()),
        );

        let mut source = source;
        // max_items = 5, page supplies 24 -> only 5 kept
        let candidates = source.refresh().await.unwrap();
        assert_eq!(candidates.len(), 5);
    }

    #[test]
    fn search_url_includes_all_parameters() {
        let source = WallhavenSource::new(&wallhaven_config(), PathBuf::from("C:/tmp/wh")).unwrap();
        let url = source.search_url(2).unwrap();
        let params: std::collections::HashMap<String, String> =
            url.query_pairs().into_owned().collect();
        assert_eq!(params.get("q").map(String::as_str), Some("aurora"));
        // Wallhaven only honours the bit-flag form, so the human-readable
        // config values are normalised before they reach the query string.
        assert_eq!(params.get("categories").map(String::as_str), Some("111"));
        assert_eq!(params.get("purity").map(String::as_str), Some("100"));
        assert_eq!(params.get("sorting").map(String::as_str), Some("toplist"));
        assert_eq!(params.get("topRange").map(String::as_str), Some("1M"));
        assert_eq!(params.get("atleast").map(String::as_str), Some("1920x1080"));
        assert_eq!(params.get("page").map(String::as_str), Some("2"));
        assert!(!params.contains_key("apikey"));
    }
}

use crate::errors::Result;
use crate::sources::{image_id, ImageCandidate, ImageSource, SourceKind};
use anyhow::Context;
use async_trait::async_trait;
use feed_rs::parser;
use regex::Regex;
use reqwest::header::CONTENT_TYPE;
use reqwest::Client;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};
use tokio::time::sleep;
use url::Url;

#[derive(Debug, Clone)]
pub struct RssSource {
    url: String,
    max_items: usize,
    download_dir: PathBuf,
    client: Client,
    html_img_regex: Regex,
}

impl RssSource {
    pub fn new(url: String, max_items: usize, download_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&download_dir)
            .with_context(|| format!("failed to create {}", download_dir.display()))?;

        Ok(Self {
            url,
            max_items,
            download_dir,
            client: shared_client().clone(),
            html_img_regex: Regex::new(r#"https?://[^"'\s>]+\.(jpg|jpeg|png|gif|bmp|webp)"#)?,
        })
    }

    fn extract_feed_image_urls(&self, body: &[u8]) -> Result<Vec<String>> {
        let feed = parser::parse(body)?;
        let mut urls = Vec::new();
        let mut seen = HashSet::new();

        for entry in feed.entries {
            for link in entry.links {
                if looks_like_image_url(&link.href) && seen.insert(link.href.clone()) {
                    urls.push(link.href);
                }
            }

            if let Some(summary) = entry.summary {
                for image_url in self.find_urls_in_text(&summary.content) {
                    if seen.insert(image_url.clone()) {
                        urls.push(image_url);
                    }
                }
            }

            if let Some(content) = entry.content {
                if let Some(body) = content.body {
                    for image_url in self.find_urls_in_text(&body) {
                        if seen.insert(image_url.clone()) {
                            urls.push(image_url);
                        }
                    }
                }
            }
        }

        Ok(urls)
    }

    fn find_urls_in_text(&self, text: &str) -> Vec<String> {
        self.html_img_regex
            .find_iter(text)
            .map(|m| m.as_str().to_string())
            .collect()
    }
}

#[async_trait]
impl ImageSource for RssSource {
    fn name(&self) -> &str {
        "rss"
    }

    fn kind(&self) -> SourceKind {
        SourceKind::Rss
    }

    async fn refresh(&mut self) -> Result<Vec<ImageCandidate>> {
        let body = self
            .client
            .get(&self.url)
            .send()
            .await
            .with_context(|| format!("failed request for {}", self.url))?
            .error_for_status()
            .with_context(|| format!("feed returned non-success for {}", self.url))?
            .bytes()
            .await
            .with_context(|| format!("failed reading feed bytes from {}", self.url))?;

        let feed_urls = self.extract_feed_image_urls(&body)?;
        let mut candidates = Vec::new();

        for image_url in feed_urls.into_iter().take(self.max_items) {
            let mtime = find_cached_image_path(&self.download_dir, &image_url)?
                .and_then(|path| fs::metadata(&path).ok())
                .and_then(|meta| meta.modified().ok());
            candidates.push(ImageCandidate::rss(
                image_id("rss", &PathBuf::from(&image_url)),
                image_url,
                self.download_dir.clone(),
                mtime,
            ));
        }

        Ok(candidates)
    }
}

pub fn find_cached_image_path(download_dir: &Path, image_url: &str) -> Result<Option<PathBuf>> {
    find_cached_image_path_by_hash(download_dir, &hash_image_url(image_url))
}

pub async fn resolve_image_path(download_dir: &Path, image_url: &str) -> Result<Option<PathBuf>> {
    let url_hash = hash_image_url(image_url);
    if let Some(path) = find_cached_image_path_by_hash(download_dir, &url_hash)? {
        return Ok(Some(path));
    }

    let download_key = format!("{}:{url_hash}", download_dir.display());
    loop {
        if let Some(path) = find_cached_image_path_by_hash(download_dir, &url_hash)? {
            return Ok(Some(path));
        }

        let Some(_guard) = try_acquire_download(download_key.clone()) else {
            sleep(Duration::from_millis(50)).await;
            continue;
        };

        if let Some(path) = find_cached_image_path_by_hash(download_dir, &url_hash)? {
            return Ok(Some(path));
        }

        return download_image_to_cache(download_dir, image_url, &url_hash).await;
    }
}

/// Proxy registered from the configuration file before the first request.
///
/// The HTTP client below is memoized on first use, so this has to be called
/// during startup — a later change only takes effect on the next launch.
static CONFIGURED_PROXY: OnceLock<Option<String>> = OnceLock::new();

/// Register the `proxy` config value used by every remote source.
pub(crate) fn configure_client_proxy(proxy: Option<String>) {
    if CONFIGURED_PROXY.set(proxy).is_err() {
        tracing::debug!("HTTP client proxy already configured; ignoring later change");
    }
}

pub(crate) fn shared_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        // reqwest's provider-neutral rustls feature keeps Linux builds on the
        // portable ring backend instead of requiring the AWS-LC C toolchain.
        // A provider may already have been installed by another TLS consumer.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut builder = Client::builder()
            .user_agent("aura/0.1 (+https://example.invalid)")
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30));

        // Explicit config wins over the environment: on networks where the
        // remote hosts are blocked the user has to name a local proxy, and an
        // inherited HTTPS_PROXY is not always the one that can reach them.
        let mut applied = false;
        if let Some(proxy_url) = CONFIGURED_PROXY
            .get()
            .and_then(|proxy| proxy.as_deref())
            .map(str::trim)
            .filter(|proxy_url| !proxy_url.is_empty())
        {
            match reqwest::Proxy::all(proxy_url) {
                Ok(proxy) => {
                    builder = builder.proxy(proxy);
                    applied = true;
                    tracing::info!(proxy = proxy_url, "remote sources will use the configured proxy");
                }
                Err(error) => {
                    tracing::warn!(
                        proxy = proxy_url,
                        error = %error,
                        "configured proxy is invalid; falling back to the environment"
                    );
                }
            }
        }

        // reqwest does not consult the HTTP(S)_PROXY environment variables by
        // default (and 0.13 dropped system-proxy discovery entirely), so read
        // them explicitly to let remote sources (RSS, Wallhaven) work behind
        // a local proxy.
        if !applied {
            for variable in ["HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"] {
                if let Ok(proxy_url) = std::env::var(variable) {
                    if let Ok(proxy) = reqwest::Proxy::all(&proxy_url) {
                        builder = builder.proxy(proxy);
                        break;
                    }
                }
            }
        }

        builder
            .build()
            .expect("RSS HTTP client must build")
    })
}

fn active_downloads() -> &'static Mutex<HashSet<String>> {
    static ACTIVE_DOWNLOADS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    ACTIVE_DOWNLOADS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn try_acquire_download(key: String) -> Option<DownloadGuard> {
    let mut active = active_downloads()
        .lock()
        .expect("RSS download lock should not be poisoned");
    if active.insert(key.clone()) {
        Some(DownloadGuard { key })
    } else {
        None
    }
}

struct DownloadGuard {
    key: String,
}

impl Drop for DownloadGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = active_downloads().lock() {
            active.remove(&self.key);
        }
    }
}

async fn download_image_to_cache(
    download_dir: &Path,
    image_url: &str,
    url_hash: &str,
) -> Result<Option<PathBuf>> {
    let response = shared_client()
        .get(image_url)
        .send()
        .await
        .with_context(|| format!("failed request for {image_url}"))?;
    let response = response
        .error_for_status()
        .with_context(|| format!("feed image url returned non-success for {image_url}"))?;

    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .map(|v| v.to_ascii_lowercase());

    if let Some(content_type) = &content_type {
        if !content_type.starts_with("image/") && !looks_like_image_url(image_url) {
            return Ok(None);
        }
    }

    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("failed reading bytes from {image_url}"))?;
    if bytes.is_empty() {
        return Ok(None);
    }

    let ext = extension_from_content_type(content_type.as_deref())
        .or_else(|| extension_from_url(image_url))
        .unwrap_or_else(|| "jpg".to_string());
    let output = download_dir.join(format!("{url_hash}.{ext}"));
    if output.exists() && output.is_file() {
        return Ok(Some(output));
    }

    tokio::fs::create_dir_all(download_dir)
        .await
        .with_context(|| format!("failed to create {}", download_dir.display()))?;

    let tmp = temp_output_path(&output);
    tokio::fs::write(&tmp, &bytes)
        .await
        .with_context(|| format!("failed to write {}", tmp.display()))?;

    if let Some(existing) = find_cached_image_path_by_hash(download_dir, url_hash)? {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Ok(Some(existing));
    }

    match tokio::fs::rename(&tmp, &output).await {
        Ok(()) => Ok(Some(output)),
        Err(error) => {
            if let Some(existing) = find_cached_image_path_by_hash(download_dir, url_hash)? {
                let _ = tokio::fs::remove_file(&tmp).await;
                Ok(Some(existing))
            } else {
                let _ = tokio::fs::remove_file(&tmp).await;
                Err(error).with_context(|| format!("failed to move {}", output.display()))
            }
        }
    }
}

fn find_cached_image_path_by_hash(download_dir: &Path, url_hash: &str) -> Result<Option<PathBuf>> {
    if !download_dir.exists() {
        return Ok(None);
    }

    let mut matches = Vec::new();
    for entry in fs::read_dir(download_dir)
        .with_context(|| format!("failed to read {}", download_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if stem == url_hash {
            matches.push(path);
        }
    }

    matches.sort();
    Ok(matches.into_iter().next())
}

fn temp_output_path(output: &Path) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let filename = output
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("rss-image");
    output.with_file_name(format!("{filename}.tmp-{unique}"))
}

fn hash_image_url(image_url: &str) -> String {
    blake3::hash(image_url.as_bytes()).to_hex().to_string()
}

fn looks_like_image_url(url: &str) -> bool {
    extension_from_url(url)
        .map(|ext| {
            matches!(
                ext.as_str(),
                "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp"
            )
        })
        .unwrap_or(false)
}

fn extension_from_url(url: &str) -> Option<String> {
    Url::parse(url).ok().and_then(|parsed| {
        Path::new(parsed.path())
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|x| x.to_ascii_lowercase())
    })
}

fn extension_from_content_type(content_type: Option<&str>) -> Option<String> {
    let content_type = content_type?;
    if content_type.starts_with("image/jpeg") || content_type.starts_with("image/jpg") {
        Some("jpg".to_string())
    } else if content_type.starts_with("image/png") {
        Some("png".to_string())
    } else if content_type.starts_with("image/gif") {
        Some("gif".to_string())
    } else if content_type.starts_with("image/webp") {
        Some("webp".to_string())
    } else if content_type.starts_with("image/bmp") {
        Some("bmp".to_string())
    } else {
        None
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    #[derive(Clone, Debug)]
    pub(crate) struct ResponseSpec {
        pub status: u16,
        pub content_type: String,
        pub body: Vec<u8>,
    }

    impl ResponseSpec {
        pub(crate) fn ok(content_type: &str, body: impl Into<Vec<u8>>) -> Self {
            Self {
                status: 200,
                content_type: content_type.to_string(),
                body: body.into(),
            }
        }
    }

    pub(crate) struct TestServer {
        base_url: String,
        responses: Arc<Mutex<HashMap<String, ResponseSpec>>>,
        hits: Arc<Mutex<HashMap<String, usize>>>,
        shutdown: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl TestServer {
        pub(crate) fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("failed to bind test server");
            listener
                .set_nonblocking(true)
                .expect("failed to set nonblocking");
            let address = listener.local_addr().expect("failed to read local address");
            let responses = Arc::new(Mutex::new(HashMap::new()));
            let hits = Arc::new(Mutex::new(HashMap::new()));
            let shutdown = Arc::new(AtomicBool::new(false));
            let thread = {
                let responses = Arc::clone(&responses);
                let hits = Arc::clone(&hits);
                let shutdown = Arc::clone(&shutdown);
                thread::spawn(move || {
                    while !shutdown.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((stream, _)) => handle_connection(stream, &responses, &hits),
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                thread::sleep(Duration::from_millis(10));
                            }
                            Err(_) => break,
                        }
                    }
                })
            };

            Self {
                base_url: format!("http://{address}"),
                responses,
                hits,
                shutdown,
                thread: Some(thread),
            }
        }

        pub(crate) fn url(&self, path: &str) -> String {
            format!("{}{path}", self.base_url)
        }

        pub(crate) fn set_response(&self, path: &str, response: ResponseSpec) {
            self.responses
                .lock()
                .expect("test server responses lock poisoned")
                .insert(path.to_string(), response);
        }

        pub(crate) fn hits(&self, path: &str) -> usize {
            self.hits
                .lock()
                .expect("test server hits lock poisoned")
                .get(path)
                .copied()
                .unwrap_or(0)
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Relaxed);
            let _ = TcpStream::connect(self.base_url.trim_start_matches("http://"));
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    fn handle_connection(
        mut stream: TcpStream,
        responses: &Arc<Mutex<HashMap<String, ResponseSpec>>>,
        hits: &Arc<Mutex<HashMap<String, usize>>>,
    ) {
        let mut buffer = [0_u8; 4096];
        let size = match stream.read(&mut buffer) {
            Ok(size) => size,
            Err(_) => return,
        };
        let request = String::from_utf8_lossy(&buffer[..size]);
        let path = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("/")
            .to_string();

        {
            let mut counts = hits.lock().expect("test server hits lock poisoned");
            *counts.entry(path.clone()).or_insert(0) += 1;
        }

        let response = responses
            .lock()
            .expect("test server responses lock poisoned")
            .get(&path)
            .cloned()
            .unwrap_or_else(|| ResponseSpec {
                status: 404,
                content_type: "text/plain".to_string(),
                body: b"not found".to_vec(),
            });

        let status_text = match response.status {
            200 => "OK",
            404 => "Not Found",
            _ => "OK",
        };
        let headers = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response.status,
            status_text,
            response.content_type,
            response.body.len()
        );
        let _ = stream.write_all(headers.as_bytes());
        let _ = stream.write_all(&response.body);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::ImageSource;
    use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba};
    use std::io::Cursor;
    use tempfile::tempdir;
    use test_support::{ResponseSpec, TestServer};

    #[test]
    fn finds_cached_image_by_url_hash_stem() {
        let tmp = tempdir().unwrap();
        let image_url = "https://example.com/photo";
        let cached = tmp
            .path()
            .join(format!("{}.webp", hash_image_url(image_url)));
        fs::write(&cached, b"cached").unwrap();

        let resolved = find_cached_image_path(tmp.path(), image_url).unwrap();
        assert_eq!(resolved, Some(cached));
    }

    #[tokio::test]
    async fn refresh_returns_rss_candidates_without_downloading_images() {
        let server = TestServer::start();
        let image_path = "/image-one.png";
        let feed_path = "/feed.xml";
        server.set_response(
            feed_path,
            ResponseSpec::ok(
                "application/rss+xml",
                feed_xml(&server.url(image_path)).into_bytes(),
            ),
        );
        server.set_response(image_path, ResponseSpec::ok("image/png", tiny_png_bytes()));

        let tmp = tempdir().unwrap();
        let mut source = RssSource::new(server.url(feed_path), 10, tmp.path().join("rss")).unwrap();

        let candidates = source.refresh().await.unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(server.hits(feed_path), 1);
        assert_eq!(server.hits(image_path), 0);
        assert!(candidates[0].cached_local_path().unwrap().is_none());
    }

    #[tokio::test]
    async fn resolve_image_path_uses_url_hash_filename_and_cache_hit() {
        let server = TestServer::start();
        let image_path = "/image";
        server.set_response(image_path, ResponseSpec::ok("image/png", tiny_png_bytes()));

        let tmp = tempdir().unwrap();
        let image_url = server.url(image_path);

        let first = resolve_image_path(tmp.path(), &image_url)
            .await
            .unwrap()
            .unwrap();
        let second = resolve_image_path(tmp.path(), &image_url)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(
            first.file_stem().and_then(|stem| stem.to_str()),
            Some(hash_image_url(&image_url).as_str())
        );
        assert_eq!(first.extension().and_then(|ext| ext.to_str()), Some("png"));
        assert_eq!(server.hits(image_path), 1);
    }

    #[tokio::test]
    async fn refresh_detects_existing_cached_file_after_feed_refetch() {
        let server = TestServer::start();
        let image_path = "/image-two.png";
        let feed_path = "/feed.xml";
        let image_url = server.url(image_path);
        server.set_response(
            feed_path,
            ResponseSpec::ok("application/rss+xml", feed_xml(&image_url).into_bytes()),
        );
        server.set_response(image_path, ResponseSpec::ok("image/png", tiny_png_bytes()));

        let tmp = tempdir().unwrap();
        let mut source = RssSource::new(server.url(feed_path), 10, tmp.path().join("rss")).unwrap();

        let first = source.refresh().await.unwrap();
        first[0].prefetch().await.unwrap();
        let second = source.refresh().await.unwrap();

        assert_eq!(server.hits(image_path), 1);
        assert!(second[0].cached_local_path().unwrap().is_some());
        assert!(second[0].mtime.is_some());
    }

    fn feed_xml(image_url: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0">
  <channel>
    <title>Aura Test Feed</title>
    <item>
      <title>One</title>
      <link>{image_url}</link>
    </item>
  </channel>
</rss>"#
        )
    }

    fn tiny_png_bytes() -> Vec<u8> {
        let image: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(1, 1, Rgba([10, 20, 30, 255]));
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }
}

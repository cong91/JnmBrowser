use reqwest::{header, Client};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::api_client::ApiClient;
use crate::browser::{create_browser, BrowserType};
use crate::browser_version_manager::DownloadInfo;
use crate::events;

// Global state to track currently downloading browser-version pairs
lazy_static::lazy_static! {
  static ref DOWNLOADING_BROWSERS: std::sync::Arc<Mutex<std::collections::HashSet<String>>> =
    std::sync::Arc::new(Mutex::new(std::collections::HashSet::new()));
  static ref DOWNLOAD_CANCELLATION_TOKENS: std::sync::Arc<Mutex<std::collections::HashMap<String, CancellationToken>>> =
    std::sync::Arc::new(Mutex::new(std::collections::HashMap::new()));
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DownloadProgress {
  pub browser: String,
  pub version: String,
  pub downloaded_bytes: u64,
  pub total_bytes: Option<u64>,
  pub percentage: f64,
  pub speed_bytes_per_sec: f64,
  pub eta_seconds: Option<f64>,
  pub stage: String, // "downloading", "extracting", "verifying"
}

pub struct Downloader {
  client: Client,
  api_client: &'static ApiClient,
  registry: &'static crate::downloaded_browsers_registry::DownloadedBrowsersRegistry,
  version_service: &'static crate::browser_version_manager::BrowserVersionManager,
  extractor: &'static crate::extraction::Extractor,
  geoip_downloader: &'static crate::geoip_downloader::GeoIPDownloader,
}

impl Downloader {
  fn new() -> Self {
    Self {
      client: Client::builder()
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_else(|_| Client::new()),
      api_client: ApiClient::instance(),
      registry: crate::downloaded_browsers_registry::DownloadedBrowsersRegistry::instance(),
      version_service: crate::browser_version_manager::BrowserVersionManager::instance(),
      extractor: crate::extraction::Extractor::instance(),
      geoip_downloader: crate::geoip_downloader::GeoIPDownloader::instance(),
    }
  }

  pub fn instance() -> &'static Downloader {
    &DOWNLOADER
  }

  #[cfg(test)]
  pub fn new_for_test() -> Self {
    Self {
      client: Client::new(),
      api_client: ApiClient::instance(),
      registry: crate::downloaded_browsers_registry::DownloadedBrowsersRegistry::instance(),
      version_service: crate::browser_version_manager::BrowserVersionManager::instance(),
      extractor: crate::extraction::Extractor::instance(),
      geoip_downloader: crate::geoip_downloader::GeoIPDownloader::instance(),
    }
  }

  #[cfg(test)]
  pub async fn download_file(
    &self,
    download_url: &str,
    dest_path: &Path,
    filename: &str,
  ) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let file_path = dest_path.join(filename);

    let response = self
      .client
      .get(download_url)
      .header(
        "User-Agent",
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
      )
      .send()
      .await?;

    if !response.status().is_success() {
      return Err(format!("Download failed with status: {}", response.status()).into());
    }

    let mut file = std::fs::OpenOptions::new()
      .create(true)
      .truncate(true)
      .write(true)
      .open(&file_path)?;

    let mut stream = response.bytes_stream();
    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
      let chunk = chunk?;
      io::copy(&mut chunk.as_ref(), &mut file)?;
    }

    Ok(file_path)
  }

  /// Resolve the actual download URL for browsers that need dynamic asset resolution
  pub async fn resolve_download_url(
    &self,
    browser_type: BrowserType,
    version: &str,
    _download_info: &DownloadInfo,
  ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    match browser_type {
      BrowserType::Camoufox => {
        // For Camoufox, verify the asset exists and find the correct download URL
        let releases = self
          .api_client
          .fetch_camoufox_releases_with_caching(true)
          .await?;

        let release = releases
          .iter()
          .find(|r| r.tag_name == version)
          .or_else(|| {
            log::info!("Camoufox: requested version {version} not found, using latest available");
            releases.first()
          })
          .ok_or("No Camoufox releases found".to_string())?;

        // Get platform and architecture info
        let (os, arch) = Self::get_platform_info();

        // Find the appropriate asset
        let asset_url = self
          .find_camoufox_asset(&release.assets, &os, &arch)
          .ok_or(format!(
            "No compatible asset found for Camoufox version {version} on {os}/{arch}"
          ))?;

        Ok(asset_url)
      }
      BrowserType::Chromium => {
        // For Chromium, get the download URL from the manifest
        let version_info = self
          .api_client
          .fetch_chromium_version_with_caching(true)
          .await?;

        if version_info.version != version {
          log::info!(
            "Chromium: requested version {version}, using available version {}",
            version_info.version
          );
        }

        // Get the download URL for current platform
        let download_url = self
          .api_client
          .get_chromium_download_url(&version_info)
          .ok_or_else(|| {
            let (os, arch) = Self::get_platform_info();
            format!(
              "No compatible download found for Chromium on {os}/{arch}. Available platforms: {}",
              version_info
                .downloads
                .iter()
                .filter_map(|(k, v)| if v.is_some() { Some(k.as_str()) } else { None })
                .collect::<Vec<_>>()
                .join(", ")
            )
          })?;

        Ok(download_url)
      }
    }
  }

  /// Get platform and architecture information
  fn get_platform_info() -> (String, String) {
    let os = if cfg!(target_os = "windows") {
      "windows"
    } else if cfg!(target_os = "linux") {
      "linux"
    } else if cfg!(target_os = "macos") {
      "macos"
    } else {
      "unknown"
    };

    let arch = if cfg!(target_arch = "x86_64") {
      "x64"
    } else if cfg!(target_arch = "aarch64") {
      "arm64"
    } else {
      "unknown"
    };

    (os.to_string(), arch.to_string())
  }

  /// Find the appropriate Camoufox asset for the current platform and architecture
  fn find_camoufox_asset(
    &self,
    assets: &[crate::browser::GithubAsset],
    os: &str,
    arch: &str,
  ) -> Option<String> {
    // Camoufox asset naming pattern: camoufox-{version}-beta.{number}-{os}.{arch}.zip
    // Example: camoufox-135.0.1-beta.24-lin.x86_64.zip
    let (os_name, arch_name) = match (os, arch) {
      ("windows", "x64") => ("win", "x86_64"),
      ("windows", "arm64") => ("win", "arm64"),
      ("linux", "x64") => ("lin", "x86_64"),
      ("linux", "arm64") => ("lin", "arm64"),
      ("macos", "x64") => ("mac", "x86_64"),
      ("macos", "arm64") => ("mac", "arm64"),
      _ => return None,
    };

    // Use ends_with for precise matching to avoid false positives
    // The separator before OS is a dash: -lin.x86_64.zip, -mac.arm64.zip, etc.
    let pattern = format!("-{os_name}.{arch_name}.zip");
    let asset = assets.iter().find(|asset| {
      let name = asset.name.to_lowercase();
      name.starts_with("camoufox-") && name.ends_with(&pattern)
    });

    if let Some(asset) = asset {
      log::info!(
        "Selected Camoufox asset for {}/{}: {}",
        os,
        arch,
        asset.name
      );
      Some(asset.browser_download_url.clone())
    } else {
      log::warn!(
        "No matching Camoufox asset found for {}/{} with pattern '{}'. Available assets: {:?}",
        os,
        arch,
        pattern,
        assets.iter().map(|a| &a.name).collect::<Vec<_>>()
      );
      None
    }
  }

  /// Ensure version.json exists in the Camoufox installation directory.
  /// Creates the file if it doesn't exist, using the version from the tag name.
  async fn ensure_camoufox_version_json(
    &self,
    browser_dir: &Path,
    version: &str,
  ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // The browser_dir is typically: binaries/camoufox/<version>/
    // Find the executable directory within it
    let version_json_locations = vec![
      browser_dir.join("version.json"),
      browser_dir.join("camoufox").join("version.json"),
    ];

    // Check if version.json already exists in any expected location
    for location in &version_json_locations {
      if location.exists() {
        log::info!("version.json already exists at: {}", location.display());
        return Ok(());
      }
    }

    // Parse the Firefox version from the Camoufox version tag
    // Format: "135.0.1-beta.24" -> Firefox version is "135.0.1" (or just "135.0")
    let firefox_version = version.split('-').next().unwrap_or(version);

    // Create version.json in the browser directory
    let version_json_path = browser_dir.join("version.json");
    let version_data = serde_json::json!({
      "version": firefox_version
    });

    let version_json_str = serde_json::to_string_pretty(&version_data)?;
    tokio::fs::write(&version_json_path, version_json_str).await?;

    log::info!(
      "Created version.json at {} with Firefox version: {}",
      version_json_path.display(),
      firefox_version
    );

    Ok(())
  }

  pub async fn download_browser<R: tauri::Runtime>(
    &self,
    _app_handle: &tauri::AppHandle<R>,
    browser_type: BrowserType,
    version: &str,
    download_info: &DownloadInfo,
    dest_path: &Path,
    cancel_token: Option<&CancellationToken>,
  ) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let file_path = dest_path.join(&download_info.filename);

    // Resolve the actual download URL
    log::info!(
      "Resolving download URL for {} {}",
      browser_type.as_str(),
      version
    );
    let download_url = self
      .resolve_download_url(browser_type.clone(), version, download_info)
      .await?;
    log::info!("Download URL resolved: {}", download_url);

    // Determine if we have a partial file to resume
    let mut existing_size: u64 = 0;
    if let Ok(meta) = std::fs::metadata(&file_path) {
      existing_size = meta.len();
    }

    // Build request with retry logic for transient network errors.
    let max_retries = 3u32;
    let mut response: Option<reqwest::Response> = None;
    for attempt in 0..=max_retries {
      let mut request = self
        .client
        .get(&download_url)
        .header(header::ACCEPT_ENCODING, "identity")
        .header(
          "User-Agent",
          "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/136.0.0.0 Safari/537.36",
        );

      if existing_size > 0 {
        request = request.header("Range", format!("bytes={existing_size}-"));
      }

      log::info!("Sending download request (attempt {})...", attempt + 1);
      match request.send().await {
        Ok(resp) => {
          log::info!(
            "Download response received: status={}, content-length={:?}",
            resp.status(),
            resp.content_length()
          );
          if resp.status().as_u16() == 416 && existing_size > 0 {
            let _ = std::fs::remove_file(&file_path);
            existing_size = 0;
            log::warn!("Download returned 416, retrying without Range header");
            continue;
          }
          response = Some(resp);
          break;
        }
        Err(e) => {
          let is_retryable = e.is_connect() || e.is_timeout() || e.is_request();
          if is_retryable && attempt < max_retries {
            let delay = 2u64.pow(attempt);
            log::warn!(
              "Download attempt {} failed ({}), retrying in {}s...",
              attempt + 1,
              e,
              delay
            );
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
          } else {
            return Err(format!("Download failed after {} attempts: {}", attempt + 1, e).into());
          }
        }
      }
    }
    let response = response.ok_or_else(|| -> Box<dyn std::error::Error + Send + Sync> {
      "Download failed: no response received".into()
    })?;

    // Check if the response is successful (200 OK or 206 Partial Content)
    if !(response.status().is_success() || response.status().as_u16() == 206) {
      return Err(format!("Download failed with status: {}", response.status()).into());
    }

    // Determine total size
    let mut total_size = response.content_length();

    // If resuming (206) and Content-Range is present, parse total
    if response.status().as_u16() == 206 {
      if let Some(content_range) = response.headers().get(reqwest::header::CONTENT_RANGE) {
        if let Ok(cr) = content_range.to_str() {
          // Format: bytes start-end/total
          if let Some((_, total_str)) = cr.split('/').collect::<Vec<_>>().split_first() {
            if let Some(total_str) = total_str.first() {
              if let Ok(total) = total_str.parse::<u64>() {
                total_size = Some(total);
              }
            }
          }
        }
      } else if let Some(len) = response.headers().get(reqwest::header::CONTENT_LENGTH) {
        // Fallback: total = existing + incoming length
        if let Ok(len_str) = len.to_str() {
          if let Ok(incoming) = len_str.parse::<u64>() {
            total_size = Some(existing_size + incoming);
          }
        }
      }
    } else if existing_size > 0 && response.status().is_success() {
      // Server ignored range or we asked from 0; if 200 and existing file has content, start fresh
      // Truncate existing file so we don't append duplicate bytes
      let _ = std::fs::remove_file(&file_path);
      existing_size = 0;
    }

    // If the existing file already matches the total size, skip the download
    if existing_size > 0 {
      if let Some(total) = total_size {
        if existing_size >= total {
          log::info!(
            "Archive {} already complete ({} bytes), skipping download",
            file_path.display(),
            existing_size
          );
          return Ok(file_path);
        }
      }
    }

    let mut downloaded = existing_size;
    let start_time = std::time::Instant::now();
    let mut last_update = start_time;

    // Emit initial progress AFTER we've established total size and resume state
    let initial_percentage = if let Some(total) = total_size {
      if total > 0 {
        (existing_size as f64 / total as f64) * 100.0
      } else {
        0.0
      }
    } else {
      0.0
    };

    let initial_stage = "downloading".to_string();

    let progress = DownloadProgress {
      browser: browser_type.as_str().to_string(),
      version: version.to_string(),
      downloaded_bytes: existing_size,
      total_bytes: total_size,
      percentage: initial_percentage,
      speed_bytes_per_sec: 0.0,
      eta_seconds: None,
      stage: initial_stage,
    };

    let _ = events::emit("download-progress", &progress);

    // Open file in append mode (resuming) or create new.
    // Wrap in BufWriter with a large buffer to reduce the number of disk writes,
    // which dramatically improves download speed on Windows (NTFS + Defender overhead).
    use std::fs::OpenOptions;
    use std::io::Write;
    let raw_file = OpenOptions::new()
      .create(true)
      .append(true)
      .open(&file_path)?;
    let mut file = io::BufWriter::with_capacity(8 * 1024 * 1024, raw_file);
    let mut stream = response.bytes_stream();

    use futures_util::StreamExt;
    while let Some(chunk) = stream.next().await {
      if let Some(token) = cancel_token {
        if token.is_cancelled() {
          drop(file);
          return Err("Download paused".into());
        }
      }
      let chunk = chunk?;
      file.write_all(&chunk)?;
      downloaded += chunk.len() as u64;

      let now = std::time::Instant::now();
      // Update progress every 100ms to avoid too many events
      if now.duration_since(last_update).as_millis() >= 100 {
        let elapsed = start_time.elapsed().as_secs_f64();
        // Compute speed based only on bytes downloaded in this session to avoid inflated values when resuming
        let downloaded_since_start = downloaded.saturating_sub(existing_size);
        let speed = if elapsed > 0.0 {
          downloaded_since_start as f64 / elapsed
        } else {
          0.0
        };
        let percentage = if let Some(total) = total_size {
          if total > 0 {
            (downloaded as f64 / total as f64) * 100.0
          } else {
            0.0
          }
        } else {
          0.0
        };
        let eta = if speed > 0.0 {
          total_size.map(|total| (total - downloaded) as f64 / speed)
        } else {
          None
        };

        let stage_description = "downloading".to_string();

        let progress = DownloadProgress {
          browser: browser_type.as_str().to_string(),
          version: version.to_string(),
          downloaded_bytes: downloaded,
          total_bytes: total_size,
          percentage,
          speed_bytes_per_sec: speed,
          eta_seconds: eta,
          stage: stage_description,
        };

        let _ = events::emit("download-progress", &progress);
        last_update = now;
      }
    }

    // Flush remaining buffered data to disk
    file.flush()?;

    Ok(file_path)
  }

  /// Download a browser binary, verify it, and register it in the downloaded browsers registry
  pub async fn download_browser_full<R: tauri::Runtime>(
    &self,
    app_handle: &tauri::AppHandle<R>,
    browser_str: String,
    version: String,
  ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let browser_key = crate::browser::canonical_browser_name(&browser_str).to_string();
    let browser_type =
      BrowserType::from_str(&browser_key).map_err(|e| format!("Invalid browser type: {e}"))?;

    // Register cancellation before any slow version-resolution network calls so the UI can pause
    // while the downloader is still preparing the real asset URL.
    let requested_download_key = format!("{browser_key}-{version}");
    let cancel_token = {
      let mut downloading = DOWNLOADING_BROWSERS.lock().unwrap();
      if downloading.contains(&requested_download_key) {
        return Err(format!("Browser '{browser_key}' version '{version}' is already being downloaded. Please wait for the current download to complete.").into());
      }
      downloading.insert(requested_download_key.clone());

      let token = CancellationToken::new();
      let mut tokens = DOWNLOAD_CANCELLATION_TOKENS.lock().unwrap();
      tokens.insert(requested_download_key.clone(), token.clone());
      token
    };

    // For Chromium/Camoufox, resolve the actual available version from the API
    let version = if crate::browser::is_chromium_browser_name(&browser_key) {
      match self
        .api_client
        .fetch_chromium_version_with_caching(true)
        .await
      {
        Ok(info) if info.version != version => {
          log::info!(
            "Chromium: requested {version}, using available {}",
            info.version
          );
          info.version
        }
        _ => version,
      }
    } else if browser_key == "camoufox" {
      match self
        .api_client
        .fetch_camoufox_releases_with_caching(true)
        .await
      {
        Ok(releases) if !releases.is_empty() && releases[0].tag_name != version => {
          log::info!(
            "Camoufox: requested {version}, using available {}",
            releases[0].tag_name
          );
          releases[0].tag_name.clone()
        }
        _ => version,
      }
    } else {
      version
    };

    if cancel_token.is_cancelled() {
      let mut downloading = DOWNLOADING_BROWSERS.lock().unwrap();
      downloading.remove(&requested_download_key);
      drop(downloading);
      let mut tokens = DOWNLOAD_CANCELLATION_TOKENS.lock().unwrap();
      tokens.remove(&requested_download_key);
      return Err("Download paused".into());
    }

    // If version resolution changed the version string, move the active lock/token to the
    // resolved key used by all subsequent cleanup and progress events.
    let mut download_key = requested_download_key.clone();
    let resolved_download_key = format!("{browser_key}-{version}");
    if resolved_download_key != requested_download_key {
      let mut downloading = DOWNLOADING_BROWSERS.lock().unwrap();
      if downloading.contains(&resolved_download_key) {
        downloading.remove(&requested_download_key);
        drop(downloading);
        let mut tokens = DOWNLOAD_CANCELLATION_TOKENS.lock().unwrap();
        tokens.remove(&requested_download_key);
        return Err(format!("Browser '{browser_key}' version '{version}' is already being downloaded. Please wait for the current download to complete.").into());
      }
      downloading.remove(&requested_download_key);
      downloading.insert(resolved_download_key.clone());
      drop(downloading);
      let mut tokens = DOWNLOAD_CANCELLATION_TOKENS.lock().unwrap();
      if let Some(token) = tokens.remove(&requested_download_key) {
        tokens.insert(resolved_download_key.clone(), token);
      }
      download_key = resolved_download_key;
    }

    let browser = create_browser(browser_type.clone());

    // Use injected registry instance

    let binaries_dir = crate::app_dirs::binaries_dir();
    let install_browser_dir = crate::browser::browser_storage_dir_name(&browser_key);
    let mut browser_dir = binaries_dir.clone();
    browser_dir.push(install_browser_dir);
    browser_dir.push(&version);

    if browser.is_version_downloaded(&version, &binaries_dir) {
      if !self.registry.is_browser_registered(&browser_key, &version) {
        if let Err(e) =
          self
            .registry
            .mark_download_completed(&browser_key, &version, browser_dir.clone())
        {
          log::warn!(
            "Warning: Could not repair missing registry entry for {} {}: {}",
            browser_key,
            version,
            e
          );
        } else if let Err(e) = self.registry.save() {
          log::warn!(
            "Warning: Could not persist repaired registry entry for {} {}: {}",
            browser_key,
            version,
            e
          );
        }
      }

      let mut downloading = DOWNLOADING_BROWSERS.lock().unwrap();
      downloading.remove(&download_key);
      drop(downloading);
      let mut tokens = DOWNLOAD_CANCELLATION_TOKENS.lock().unwrap();
      tokens.remove(&download_key);
      return Ok(version);
    }

    // Check if registry thinks it's downloaded, but also verify files actually exist
    if self.registry.is_browser_downloaded(&browser_key, &version) {
      // Remove from downloading set since it's already downloaded
      let mut downloading = DOWNLOADING_BROWSERS.lock().unwrap();
      downloading.remove(&download_key);
      drop(downloading);
      let mut tokens = DOWNLOAD_CANCELLATION_TOKENS.lock().unwrap();
      tokens.remove(&download_key);
      return Ok(version);
    }

    // Check if browser is supported on current platform before attempting download
    if !self
      .version_service
      .is_browser_supported(&browser_key)
      .unwrap_or(false)
    {
      // Remove from downloading set on error
      let mut downloading = DOWNLOADING_BROWSERS.lock().unwrap();
      downloading.remove(&download_key);
      drop(downloading);
      let mut tokens = DOWNLOAD_CANCELLATION_TOKENS.lock().unwrap();
      tokens.remove(&download_key);
      return Err(
        format!(
          "Browser '{}' is not supported on your platform ({} {}). Supported browsers: {}",
          browser_key,
          std::env::consts::OS,
          std::env::consts::ARCH,
          self.version_service.get_supported_browsers().join(", ")
        )
        .into(),
      );
    }

    let download_info = self
      .version_service
      .get_download_info(&browser_key, &version)
      .map_err(|e| format!("Failed to get download info: {e}"))?;

    // Create browser directory. Legacy API callers may still pass the old alias,
    // but the actual replacement engine is stored
    // under binaries/fingerprint-chromium/<version>/.
    std::fs::create_dir_all(&browser_dir)
      .map_err(|e| format!("Failed to create browser directory: {e}"))?;

    // Mark download as started (but don't add to registry yet)
    self
      .registry
      .mark_download_started(&browser_key, &version, browser_dir.clone());

    // Attempt to download the archive. If the download fails but an archive with the
    // expected filename already exists (manual download), continue using that file.
    let download_path: PathBuf = match self
      .download_browser(
        app_handle,
        browser_type.clone(),
        &version,
        &download_info,
        &browser_dir,
        Some(&cancel_token),
      )
      .await
    {
      Ok(path) => path,
      Err(e) => {
        // Do NOT continue with extraction on failed downloads. Partial files may exist but are invalid.
        // Clean registry entry and stop here so the UI can show a single, clear error.
        let _ = self.registry.remove_browser(&browser_key, &version);
        let _ = self.registry.save();
        let mut downloading = DOWNLOADING_BROWSERS.lock().unwrap();
        downloading.remove(&download_key);
        drop(downloading);
        let mut tokens = DOWNLOAD_CANCELLATION_TOKENS.lock().unwrap();
        tokens.remove(&download_key);

        // Emit cancelled stage if the download was cancelled by user
        if cancel_token.is_cancelled() {
          let progress = DownloadProgress {
            browser: browser_key.clone(),
            version: version.clone(),
            downloaded_bytes: 0,
            total_bytes: None,
            percentage: 0.0,
            speed_bytes_per_sec: 0.0,
            eta_seconds: None,
            stage: "cancelled".to_string(),
          };
          let _ = events::emit("download-progress", &progress);
        }

        return Err(e);
      }
    };

    // Use the extraction module
    if download_info.is_archive {
      match self
        .extractor
        .extract_browser(
          app_handle,
          browser_type.clone(),
          &version,
          &download_path,
          &browser_dir,
        )
        .await
      {
        Ok(_) => {
          // Do not remove the archive here. We keep it until verification succeeds.
        }
        Err(e) => {
          log::error!("Extraction failed for {browser_key} {version}: {e}");

          // Delete the corrupt/invalid archive so a fresh download happens next time
          if download_path.exists() {
            log::info!("Deleting corrupt archive: {}", download_path.display());
            let _ = std::fs::remove_file(&download_path);
          }

          let _ = self.registry.remove_browser(&browser_key, &version);
          let _ = self.registry.save();
          {
            let mut downloading = DOWNLOADING_BROWSERS.lock().unwrap();
            downloading.remove(&download_key);
          }
          {
            let mut tokens = DOWNLOAD_CANCELLATION_TOKENS.lock().unwrap();
            tokens.remove(&download_key);
          }

          // Emit error stage so the UI shows a toast
          let progress = DownloadProgress {
            browser: browser_key.clone(),
            version: version.clone(),
            downloaded_bytes: 0,
            total_bytes: None,
            percentage: 0.0,
            speed_bytes_per_sec: 0.0,
            eta_seconds: None,
            stage: "error".to_string(),
          };
          let _ = events::emit("download-progress", &progress);

          return Err(format!("Failed to extract browser: {e}").into());
        }
      }

      // Give filesystem a moment to settle after extraction
      tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    // Emit verification progress
    let progress = DownloadProgress {
      browser: browser_key.clone(),
      version: version.clone(),
      downloaded_bytes: 0,
      total_bytes: None,
      percentage: 100.0,
      speed_bytes_per_sec: 0.0,
      eta_seconds: None,
      stage: "verifying".to_string(),
    };
    let _ = events::emit("download-progress", &progress);

    // Verify the browser was downloaded correctly
    log::info!("Verifying download for browser: {browser_key}, version: {version}");

    // Use the browser's own verification method
    if !browser.is_version_downloaded(&version, &binaries_dir) {
      // Provide detailed error information for debugging
      let browser_dir = binaries_dir.join(install_browser_dir).join(&version);
      let mut error_details = format!(
        "Browser download completed but verification failed for {} {}. Expected directory: {}",
        browser_key,
        version,
        browser_dir.display()
      );

      // List what files actually exist
      if browser_dir.exists() {
        error_details.push_str("\nFiles found in directory:");
        if let Ok(entries) = std::fs::read_dir(&browser_dir) {
          for entry in entries.flatten() {
            let path = entry.path();
            let file_type = if path.is_dir() { "DIR" } else { "FILE" };
            error_details.push_str(&format!("\n  {} {}", file_type, path.display()));
          }
        } else {
          error_details.push_str("\n  (Could not read directory contents)");
        }
      } else {
        error_details.push_str("\nDirectory does not exist!");
      }

      // For Camoufox on Linux, provide specific expected files
      if browser_key == "camoufox" && cfg!(target_os = "linux") {
        let camoufox_subdir = browser_dir.join("camoufox");
        error_details.push_str("\nExpected Camoufox executable locations:");
        error_details.push_str(&format!("\n  {}/camoufox-bin", camoufox_subdir.display()));
        error_details.push_str(&format!("\n  {}/camoufox", camoufox_subdir.display()));

        if camoufox_subdir.exists() {
          error_details.push_str(&format!(
            "\nCamoufox subdirectory exists: {}",
            camoufox_subdir.display()
          ));
          if let Ok(entries) = std::fs::read_dir(&camoufox_subdir) {
            error_details.push_str("\nFiles in camoufox subdirectory:");
            for entry in entries.flatten() {
              let path = entry.path();
              let file_type = if path.is_dir() { "DIR" } else { "FILE" };
              error_details.push_str(&format!("\n  {} {}", file_type, path.display()));
            }
          }
        } else {
          error_details.push_str(&format!(
            "\nCamoufox subdirectory does not exist: {}",
            camoufox_subdir.display()
          ));
        }
      }

      // Do not delete files on verification failure; keep archive for manual retry.
      let _ = self.registry.remove_browser(&browser_key, &version);
      let _ = self.registry.save();
      // Remove browser-version pair from downloading set on verification failure
      {
        let mut downloading = DOWNLOADING_BROWSERS.lock().unwrap();
        downloading.remove(&download_key);
      }
      {
        let mut tokens = DOWNLOAD_CANCELLATION_TOKENS.lock().unwrap();
        tokens.remove(&download_key);
      }
      return Err(error_details.into());
    }

    // Mark completion in registry - only now add to registry after verification
    if let Err(e) =
      self
        .registry
        .mark_download_completed(&browser_key, &version, browser_dir.clone())
    {
      log::warn!("Warning: Could not mark {browser_key} {version} as completed in registry: {e}");
    }
    self
      .registry
      .save()
      .map_err(|e| format!("Failed to save registry: {e}"))?;

    // Now that verification succeeded, remove the archive file if it exists
    if download_info.is_archive {
      let archive_path = browser_dir.join(&download_info.filename);
      if archive_path.exists() {
        if let Err(e) = std::fs::remove_file(&archive_path) {
          log::warn!("Warning: Could not delete archive file after verification: {e}");
        }
      }
    }

    // If this is Camoufox, automatically download GeoIP database and create version.json
    if browser_key == "camoufox" {
      // Check if GeoIP database is already available
      if !crate::geoip_downloader::GeoIPDownloader::is_geoip_database_available() {
        log::info!("Downloading GeoIP database for Camoufox...");

        match self
          .geoip_downloader
          .download_geoip_database(app_handle)
          .await
        {
          Ok(_) => {
            log::info!("GeoIP database downloaded successfully");
          }
          Err(e) => {
            log::error!("Failed to download GeoIP database: {e}");
            // Don't fail the browser download if GeoIP download fails
          }
        }
      } else {
        log::info!("GeoIP database already available");
      }

      // Create version.json if it doesn't exist
      if let Err(e) = self
        .ensure_camoufox_version_json(&browser_dir, &version)
        .await
      {
        log::warn!("Failed to create version.json for Camoufox: {e}");
      }
    }

    // Emit completion
    let progress = DownloadProgress {
      browser: browser_key.clone(),
      version: version.clone(),
      downloaded_bytes: 0,
      total_bytes: None,
      percentage: 100.0,
      speed_bytes_per_sec: 0.0,
      eta_seconds: Some(0.0),
      stage: "completed".to_string(),
    };
    let _ = events::emit("download-progress", &progress);

    // Remove browser-version pair from downloading set and cancel token
    {
      let mut downloading = DOWNLOADING_BROWSERS.lock().unwrap();
      downloading.remove(&download_key);
    }
    {
      let mut tokens = DOWNLOAD_CANCELLATION_TOKENS.lock().unwrap();
      tokens.remove(&download_key);
    }

    // Auto-update non-running profiles to the latest installed version and cleanup unused binaries
    {
      let app_handle_for_update = app_handle.clone();
      tauri::async_runtime::spawn(async move {
        let auto_updater = crate::auto_updater::AutoUpdater::instance();
        match auto_updater.update_profiles_to_latest_installed(&app_handle_for_update) {
          Ok(updated) => {
            if !updated.is_empty() {
              log::info!(
                "Auto-updated {} profiles to latest installed versions: {:?}",
                updated.len(),
                updated
              );
            }
          }
          Err(e) => {
            log::error!("Failed to auto-update profile versions: {e}");
          }
        }

        let registry = crate::downloaded_browsers_registry::DownloadedBrowsersRegistry::instance();
        match registry.cleanup_unused_binaries() {
          Ok(cleaned) => {
            if !cleaned.is_empty() {
              log::info!("Cleaned up unused binaries after download: {:?}", cleaned);
            }
          }
          Err(e) => {
            log::error!("Failed to cleanup unused binaries: {e}");
          }
        }
      });
    }

    Ok(version)
  }
}

/// Check if a specific browser-version pair is currently being downloaded
pub fn is_downloading(browser: &str, version: &str) -> bool {
  let download_key = format!("{browser}-{version}");
  let downloading = DOWNLOADING_BROWSERS.lock().unwrap();
  downloading.contains(&download_key)
}

#[tauri::command]
pub async fn download_browser<R: tauri::Runtime>(
  app_handle: tauri::AppHandle<R>,
  browser_str: String,
  version: String,
) -> Result<String, String> {
  let downloader = Downloader::instance();
  downloader
    .download_browser_full(&app_handle, browser_str, version)
    .await
    .map_err(|e| format!("Failed to download browser: {e}"))
}

#[tauri::command]
pub async fn cancel_download(browser_str: String, version: String) -> Result<(), String> {
  let browser_key = crate::browser::canonical_browser_name(&browser_str).to_string();
  let download_key = format!("{browser_key}-{version}");
  let token = {
    let tokens = DOWNLOAD_CANCELLATION_TOKENS.lock().unwrap();
    tokens.get(&download_key).cloned()
  };

  if let Some(token) = token {
    token.cancel();
    Ok(())
  } else {
    Err(format!(
      "No active download found for {browser_key} {version}"
    ))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::Arc;
  use tempfile::TempDir;
  use wiremock::matchers::{method, path};
  use wiremock::{Mock, MockServer, ResponseTemplate};

  #[tokio::test]
  async fn test_download_file_with_progress() {
    let server = MockServer::start().await;
    let downloader = Downloader::new_for_test();

    let temp_dir = TempDir::new().unwrap();
    let dest_path = temp_dir.path();

    let test_content = b"This is a test file content for download simulation";

    Mock::given(method("GET"))
      .and(path("/test-download"))
      .respond_with(
        ResponseTemplate::new(200)
          .set_body_bytes(test_content)
          .insert_header("content-length", test_content.len().to_string())
          .insert_header("content-type", "application/octet-stream"),
      )
      .mount(&server)
      .await;

    let download_url = format!("{}/test-download", server.uri());

    let result = downloader
      .download_file(&download_url, dest_path, "test-file.dmg")
      .await;

    assert!(result.is_ok());
    let downloaded_file = result.unwrap();
    assert!(downloaded_file.exists());

    let downloaded_content = std::fs::read(&downloaded_file).unwrap();
    assert_eq!(downloaded_content, test_content);
  }

  #[tokio::test]
  async fn test_download_file_network_error() {
    let server = MockServer::start().await;
    let downloader = Downloader::new_for_test();

    let temp_dir = TempDir::new().unwrap();
    let dest_path = temp_dir.path();

    Mock::given(method("GET"))
      .and(path("/missing-file"))
      .respond_with(ResponseTemplate::new(404))
      .mount(&server)
      .await;

    let download_url = format!("{}/missing-file", server.uri());

    let result = downloader
      .download_file(&download_url, dest_path, "missing-file.dmg")
      .await;

    assert!(result.is_err());
  }

  #[tokio::test]
  async fn test_download_file_chunked_response() {
    let server = MockServer::start().await;
    let downloader = Downloader::new_for_test();

    let temp_dir = TempDir::new().unwrap();
    let dest_path = temp_dir.path();

    let test_content = vec![42u8; 1024]; // 1KB of data

    Mock::given(method("GET"))
      .and(path("/chunked-download"))
      .respond_with(
        ResponseTemplate::new(200)
          .set_body_bytes(test_content.clone())
          .insert_header("content-length", test_content.len().to_string())
          .insert_header("content-type", "application/octet-stream"),
      )
      .mount(&server)
      .await;

    let download_url = format!("{}/chunked-download", server.uri());

    let result = downloader
      .download_file(&download_url, dest_path, "chunked-file.dmg")
      .await;

    assert!(result.is_ok());
    let downloaded_file = result.unwrap();
    assert!(downloaded_file.exists());

    let downloaded_content = std::fs::read(&downloaded_file).unwrap();
    assert_eq!(downloaded_content.len(), test_content.len());
  }

  #[cfg(target_os = "macos")]
  struct EnvVarGuard {
    key: String,
    previous: Option<String>,
  }

  #[cfg(target_os = "macos")]
  impl EnvVarGuard {
    fn set(key: &str, value: &str) -> Self {
      let previous = std::env::var(key).ok();
      std::env::set_var(key, value);
      Self {
        key: key.to_string(),
        previous,
      }
    }
  }

  #[cfg(target_os = "macos")]
  impl Drop for EnvVarGuard {
    fn drop(&mut self) {
      if let Some(previous) = &self.previous {
        std::env::set_var(&self.key, previous);
      } else {
        std::env::remove_var(&self.key);
      }
    }
  }

  #[cfg(target_os = "macos")]
  async fn spawn_fingerprint_manifest_server(
    version: &str,
    dmg_bytes: Vec<u8>,
  ) -> (
    String,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
  ) {
    use axum::body::Body;
    use axum::http::{header, HeaderValue, Response, StatusCode};
    use axum::routing::get;
    use axum::Router;

    let manifest_hits = Arc::new(AtomicUsize::new(0));
    let dmg_hits = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");
    let manifest_json = serde_json::json!({
      "version": version,
      "downloads": {
        "linux-x64": serde_json::Value::Null,
        "linux-arm64": serde_json::Value::Null,
        "macos-x64": format!("{base_url}/test-browser.dmg"),
        "macos-arm64": format!("{base_url}/test-browser.dmg"),
        "windows-x64": serde_json::Value::Null,
        "windows-arm64": serde_json::Value::Null
      }
    })
    .to_string();

    let manifest_hits_for_route = Arc::clone(&manifest_hits);
    let manifest_json_for_route = manifest_json.clone();
    let dmg_hits_for_route = Arc::clone(&dmg_hits);
    let dmg_bytes_for_route = dmg_bytes.clone();

    let router = Router::new()
      .route(
        "/fingerprint-chromium.json",
        get(move || {
          let manifest_hits = Arc::clone(&manifest_hits_for_route);
          let manifest_json = manifest_json_for_route.clone();
          async move {
            manifest_hits.fetch_add(1, Ordering::SeqCst);
            (
              StatusCode::OK,
              [(header::CONTENT_TYPE, "application/json")],
              manifest_json,
            )
          }
        }),
      )
      .route(
        "/test-browser.dmg",
        get(move || {
          let dmg_hits = Arc::clone(&dmg_hits_for_route);
          let dmg_bytes = dmg_bytes_for_route.clone();
          async move {
            dmg_hits.fetch_add(1, Ordering::SeqCst);
            let mut response = Response::new(Body::from(dmg_bytes));
            *response.status_mut() = StatusCode::OK;
            response.headers_mut().insert(
              header::CONTENT_TYPE,
              HeaderValue::from_static("application/x-apple-diskimage"),
            );
            response
          }
        }),
      );

    let server = tokio::spawn(async move {
      axum::serve(listener, router).await.unwrap();
    });

    (
      format!("{base_url}/fingerprint-chromium.json"),
      manifest_hits,
      dmg_hits,
      server,
    )
  }

  #[cfg(target_os = "macos")]
  fn build_test_chromium_dmg(temp_dir: &TempDir) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let source_root = temp_dir.path().join("dmg-source");
    let app_root = source_root.join("Chromium.app");
    let macos_dir = app_root.join("Contents").join("MacOS");
    std::fs::create_dir_all(&macos_dir).unwrap();

    let executable = macos_dir.join("Chromium");
    std::fs::write(
      &executable,
      "#!/bin/sh\necho 'fingerprint-chromium test binary'\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&executable, permissions).unwrap();

    let dmg_path = temp_dir.path().join("fingerprint-test.dmg");
    let output = std::process::Command::new("hdiutil")
      .args([
        "create",
        "-quiet",
        "-fs",
        "HFS+",
        "-volname",
        "FingerprintChromiumTest",
        "-srcfolder",
        source_root.to_str().unwrap(),
        dmg_path.to_str().unwrap(),
      ])
      .output()
      .unwrap();

    assert!(
      output.status.success(),
      "Failed to create test DMG: stdout={}, stderr={}",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );

    dmg_path
  }

  #[cfg(target_os = "macos")]
  #[tokio::test]
  #[serial_test::serial]
  async fn test_chromium_manifest_download_reinstalls_after_deletion_and_repairs_registry() {
    let data_dir = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    let artifact_dir = TempDir::new().unwrap();
    let _data_guard = crate::app_dirs::set_test_data_dir(data_dir.path().to_path_buf());
    let _cache_guard = crate::app_dirs::set_test_cache_dir(cache_dir.path().to_path_buf());

    let version = format!("142.0.7444.175-phase3-{}", std::process::id());
    let dmg_path = build_test_chromium_dmg(&artifact_dir);
    let dmg_bytes = std::fs::read(&dmg_path).unwrap();
    let (manifest_url, _manifest_hits, dmg_hits, server) =
      spawn_fingerprint_manifest_server(&version, dmg_bytes).await;
    let _manifest_guard = EnvVarGuard::set("JNM_FINGERPRINT_CHROMIUM_MANIFEST_URL", &manifest_url);

    let registry = crate::downloaded_browsers_registry::DownloadedBrowsersRegistry::instance();
    registry.remove_browser("chromium", &version);
    registry.save().unwrap();

    let app = tauri::test::mock_app();
    let app_handle = app.handle().clone();
    let downloader = Downloader::new_for_test();
    let install_dir = crate::app_dirs::binaries_dir()
      .join("fingerprint-chromium")
      .join(&version);
    let app_bundle = install_dir.join("Chromium.app");

    let first = downloader
      .download_browser_full(&app_handle, "chromium".to_string(), version.clone())
      .await
      .unwrap();
    assert_eq!(first, version);
    assert!(app_bundle.exists());
    assert!(registry.is_browser_downloaded("chromium", &version));

    std::fs::remove_dir_all(&install_dir).unwrap();
    assert!(!install_dir.exists());

    let second = downloader
      .download_browser_full(&app_handle, "chromium".to_string(), version.clone())
      .await
      .unwrap();
    assert_eq!(second, version);
    assert!(app_bundle.exists());
    assert!(registry.is_browser_downloaded("chromium", &version));

    registry.remove_browser("chromium", &version);
    registry.save().unwrap();
    assert!(!registry.is_browser_registered("chromium", &version));

    let third = downloader
      .download_browser_full(&app_handle, "chromium".to_string(), version.clone())
      .await
      .unwrap();
    assert_eq!(third, version);
    assert!(app_bundle.exists());
    assert!(registry.is_browser_registered("chromium", &version));
    assert_eq!(
      dmg_hits.load(Ordering::SeqCst),
      2,
      "third call should repair registry from installed files instead of re-downloading archive"
    );

    server.abort();
  }
}

// Global singleton instance
lazy_static::lazy_static! {
  static ref DOWNLOADER: Downloader = Downloader::new();
}

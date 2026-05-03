use crate::browser_runner::BrowserRunner;
use crate::profile::BrowserProfile;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tauri::AppHandle;
use tokio::process::Command as TokioCommand;
use tokio::sync::Mutex as AsyncMutex;
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChromiumConfig {
  #[serde(default)]
  pub fingerprint: Option<String>,
  #[serde(default)]
  pub randomize_fingerprint_on_launch: Option<bool>,
  #[serde(default)]
  pub os: Option<String>,
  #[serde(default)]
  pub screen_max_width: Option<u32>,
  #[serde(default)]
  pub screen_max_height: Option<u32>,
  #[serde(default)]
  pub screen_min_width: Option<u32>,
  #[serde(default)]
  pub screen_min_height: Option<u32>,
  #[serde(default)]
  pub geoip: Option<serde_json::Value>, // For compatibility with shared config form
  #[serde(default)]
  pub block_images: Option<bool>, // For compatibility with shared config form
  #[serde(default)]
  pub block_webrtc: Option<bool>,
  #[serde(default)]
  pub block_webgl: Option<bool>,
  #[serde(default, skip_serializing)]
  pub proxy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct ChromiumLaunchResult {
  pub id: String,
  #[serde(alias = "process_id")]
  pub processId: Option<u32>,
  #[serde(alias = "profile_path")]
  pub profilePath: Option<String>,
  pub url: Option<String>,
  pub cdp_port: Option<u16>,
}

struct ChromiumInstance {
  id: String,
  process_id: Option<u32>,
  profile_path: Option<String>,
  url: Option<String>,
  cdp_port: Option<u16>,
}

struct ChromiumManagerInner {
  instances: HashMap<String, ChromiumInstance>,
}

pub struct ChromiumManager {
  inner: Arc<AsyncMutex<ChromiumManagerInner>>,
  http_client: Client,
}

#[derive(Debug, Deserialize)]
struct CdpTarget {
  #[serde(rename = "type")]
  target_type: String,
  #[serde(rename = "webSocketDebuggerUrl")]
  websocket_debugger_url: Option<String>,
}

impl ChromiumManager {
  fn preserved_fixed_fingerprint_fields(fingerprint: &Value) -> serde_json::Map<String, Value> {
    const KEYS: &[&str] = &[
      "userAgent",
      "platform",
      "platformVersion",
      "browserBrand",
      "brand",
      "browserVersion",
      "brandVersion",
      "hardwareConcurrency",
      "deviceMemory",
      "language",
      "languages",
      "timezone",
      "webglVendor",
      "webglRenderer",
      "gpuVendor",
      "gpuRenderer",
    ];

    let mut preserved = serde_json::Map::new();
    for key in KEYS {
      if let Some(value) = fingerprint.get(*key) {
        preserved.insert((*key).to_string(), value.clone());
      }
    }
    preserved
  }

  fn resolved_seed(profile: &BrowserProfile, config: &ChromiumConfig) -> u32 {
    if config.randomize_fingerprint_on_launch == Some(true) {
      let random_seed = rand::random::<u32>();
      if random_seed == 0 {
        1
      } else {
        random_seed
      }
    } else {
      config
        .fingerprint
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .map(|parsed| parsed.get("fingerprint").cloned().unwrap_or(parsed))
        .and_then(|existing| existing.get("seed").and_then(|value| value.as_u64()))
        .and_then(|seed| u32::try_from(seed).ok())
        .filter(|seed| *seed != 0)
        .unwrap_or_else(|| Self::stable_fingerprint_seed(profile))
    }
  }

  fn chromium_extension_launch_args(extension_paths: &[String]) -> Vec<String> {
    if extension_paths.is_empty() {
      return Vec::new();
    }

    let joined = extension_paths.join(",");
    vec![
      format!("--load-extension={joined}"),
      format!("--disable-extensions-except={joined}"),
    ]
  }

  fn new() -> Self {
    Self {
      inner: Arc::new(AsyncMutex::new(ChromiumManagerInner {
        instances: HashMap::new(),
      })),
      http_client: Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .expect("Failed to build reqwest client for chromium_manager"),
    }
  }

  pub fn instance() -> &'static ChromiumManager {
    &CHROMIUM_MANAGER
  }

  #[allow(dead_code)]
  pub fn get_profiles_dir(&self) -> PathBuf {
    crate::app_dirs::profiles_dir()
  }

  #[allow(dead_code)]
  fn get_binaries_dir(&self) -> PathBuf {
    crate::app_dirs::binaries_dir()
  }

  async fn find_free_port() -> Result<u16, Box<dyn std::error::Error + Send + Sync>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
  }

  /// Normalize fingerprint data from the legacy Chromium CDP format to our storage format.
  /// The legacy runtime returned fields like fonts and webglParameters as JSON strings, which we keep as-is.
  #[allow(dead_code)]
  fn normalize_fingerprint(fingerprint: serde_json::Value) -> serde_json::Value {
    // Our storage format matches what the legacy runtime returned:
    // - fonts, plugins, mimeTypes, voices are JSON strings
    // - webglParameters, webgl2Parameters, etc. are JSON strings
    // The form displays them as JSON text areas, so no conversion needed.
    fingerprint
  }

  /// Denormalize fingerprint data from our storage format to the legacy Chromium CDP format.
  /// The legacy runtime expects certain fields as JSON strings.
  #[allow(dead_code)]
  fn denormalize_fingerprint(fingerprint: serde_json::Value) -> serde_json::Value {
    // Our storage format matches what the legacy runtime expects:
    // - fonts, plugins, mimeTypes, voices are JSON strings
    // - webglParameters, webgl2Parameters, etc. are JSON strings
    // So no conversion is needed
    fingerprint
  }

  fn current_fingerprint_platform() -> &'static str {
    if cfg!(target_os = "macos") {
      "macos"
    } else if cfg!(target_os = "linux") {
      "linux"
    } else {
      "windows"
    }
  }

  fn stable_fingerprint_seed(profile: &BrowserProfile) -> u32 {
    let digest = Sha256::digest(profile.id.to_string().as_bytes());
    u32::from_le_bytes([digest[0], digest[1], digest[2], digest[3]])
  }

  fn json_string(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
      value
        .get(*key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
    })
  }

  fn json_u32(value: &serde_json::Value, keys: &[&str]) -> Option<u32> {
    keys.iter().find_map(|key| {
      value
        .get(*key)
        .and_then(|v| v.as_u64())
        .and_then(|n| u32::try_from(n).ok())
    })
  }

  fn json_f64(value: &serde_json::Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| {
      value
        .get(*key)
        .and_then(|v| v.as_f64().or_else(|| v.as_u64().map(|n| n as f64)))
        .filter(|n| n.is_finite())
    })
  }

  fn json_languages(value: &serde_json::Value) -> Option<String> {
    let languages = value.get("languages")?;
    if let Some(s) = languages.as_str() {
      let trimmed = s.trim();
      if !trimmed.is_empty() {
        return Some(trimmed.to_string());
      }
    }

    if let Some(arr) = languages.as_array() {
      let values = arr
        .iter()
        .filter_map(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
      if !values.is_empty() {
        return Some(values.join(","));
      }
    }

    None
  }

  fn stored_fingerprint_value(config: &ChromiumConfig) -> Value {
    config
      .fingerprint
      .as_deref()
      .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
      .map(|parsed| parsed.get("fingerprint").cloned().unwrap_or(parsed))
      .unwrap_or_else(|| json!({}))
  }

  fn fingerprint_seed(config: &ChromiumConfig, profile: &BrowserProfile) -> u32 {
    Self::stored_fingerprint_value(config)
      .get("seed")
      .and_then(|value| value.as_u64())
      .and_then(|seed| u32::try_from(seed).ok())
      .filter(|seed| *seed != 0)
      .unwrap_or_else(|| Self::stable_fingerprint_seed(profile))
  }

  fn runtime_browser_full_version(profile: &BrowserProfile) -> String {
    let version = profile.version.trim();
    if version.is_empty() {
      "142.0.7444.175".to_string()
    } else {
      version.to_string()
    }
  }

  fn runtime_browser_major_version(profile: &BrowserProfile) -> String {
    Self::runtime_browser_full_version(profile)
      .split('.')
      .next()
      .filter(|s| !s.is_empty())
      .unwrap_or("142")
      .to_string()
  }

  fn runtime_user_agent_version(profile: &BrowserProfile) -> String {
    format!("{}.0.0.0", Self::runtime_browser_major_version(profile))
  }

  fn normalize_user_agent_for_runtime(raw: &str, profile: &BrowserProfile) -> String {
    let marker = "Chrome/";
    if let Some(start) = raw.find(marker) {
      let version_start = start + marker.len();
      let version_end = raw[version_start..]
        .find(|c: char| c.is_ascii_whitespace())
        .map(|offset| version_start + offset)
        .unwrap_or_else(|| raw.len());
      let mut normalized = String::with_capacity(raw.len() + 16);
      normalized.push_str(&raw[..version_start]);
      normalized.push_str(&Self::runtime_user_agent_version(profile));
      normalized.push_str(&raw[version_end..]);
      normalized
    } else {
      raw.to_string()
    }
  }

  fn user_agent_override_params(profile: &BrowserProfile, fingerprint: &Value) -> Option<Value> {
    let user_agent = Self::json_string(fingerprint, &["userAgent", "user_agent"])
      .map(|raw| Self::normalize_user_agent_for_runtime(&raw, profile))?;

    let accept_language = Self::json_languages(fingerprint);
    let platform = Self::json_string(
      fingerprint,
      &["platform", "navigatorPlatform", "navigator_platform"],
    );

    let mut params = serde_json::Map::new();
    params.insert("userAgent".to_string(), json!(user_agent));
    if let Some(accept_language) = accept_language {
      params.insert("acceptLanguage".to_string(), json!(accept_language));
    }
    if let Some(platform) = platform {
      params.insert("platform".to_string(), json!(platform));
    }

    Some(Value::Object(params))
  }

  fn fingerprint_override_script(profile: &BrowserProfile, fingerprint: &Value) -> Option<String> {
    let mut overrides = Vec::new();
    let mut override_entries = Vec::new();

    if let Some(user_agent) = Self::json_string(fingerprint, &["userAgent", "user_agent"]) {
      let user_agent = Self::normalize_user_agent_for_runtime(&user_agent, profile);
      let app_version = user_agent
        .strip_prefix("Mozilla/")
        .unwrap_or(&user_agent)
        .to_string();
      let user_agent_json = serde_json::to_string(&user_agent).ok()?;
      let app_version_json = serde_json::to_string(&app_version).ok()?;
      overrides.push(format!(
        "overrideValue('userAgent', {user_agent_json});overrideValue('appVersion', {app_version_json});"
      ));
      override_entries.push(("userAgent".to_string(), user_agent_json));
      override_entries.push(("appVersion".to_string(), app_version_json));
    }

    if let Some(platform) = Self::json_string(
      fingerprint,
      &["platform", "navigatorPlatform", "navigator_platform"],
    ) {
      let platform_json = serde_json::to_string(&platform).ok()?;
      overrides.push(format!("overrideValue('platform', {platform_json});"));
      override_entries.push(("platform".to_string(), platform_json));
    }

    if let Some(language) = Self::json_string(fingerprint, &["language", "locale"]) {
      let language_json = serde_json::to_string(&language).ok()?;
      overrides.push(format!("overrideValue('language', {language_json});"));
      override_entries.push(("language".to_string(), language_json));
    }

    if let Some(languages) = Self::json_languages(fingerprint) {
      let languages_vec = languages
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>();
      if !languages_vec.is_empty() {
        let languages_json = serde_json::to_string(&languages_vec).ok()?;
        overrides.push(format!("overrideValue('languages', {languages_json});"));
        override_entries.push(("languages".to_string(), languages_json));
      }
    }

    if let Some(hardware_concurrency) = Self::json_u32(
      fingerprint,
      &[
        "hardwareConcurrency",
        "hardware_concurrency",
        "cpuCores",
        "cpu_cores",
      ],
    ) {
      let value = serde_json::to_string(&hardware_concurrency).ok()?;
      overrides.push(format!("overrideValue('hardwareConcurrency', {value});"));
      override_entries.push(("hardwareConcurrency".to_string(), value));
    }

    if let Some(device_memory) = Self::json_f64(fingerprint, &["deviceMemory", "device_memory"]) {
      let value = serde_json::to_string(&device_memory).ok()?;
      overrides.push(format!("overrideValue('deviceMemory', {value});"));
      override_entries.push(("deviceMemory".to_string(), value));
    }

    if overrides.is_empty() {
      return None;
    }

    let overrides_object = override_entries
      .into_iter()
      .map(|(key, value)| {
        serde_json::to_string(&key)
          .ok()
          .map(|serialized_key| format!("{serialized_key}:{value}"))
      })
      .collect::<Option<Vec<_>>>()?
      .join(",");

    Some(format!(
      "(function(){{const nav=window.navigator;const proto=Object.getPrototypeOf(nav);const define=(target,key,getter)=>{{if(!target)return false;try{{Object.defineProperty(target,key,{{configurable:true,get:getter}});return true;}}catch(_e){{return false;}}}};const overrideValue=(key,value)=>{{const getter=()=>value;define(nav,key,getter);define(proto,key,getter);}};{}const overrides={{{}}};const overriddenKeys=new Set(Object.keys(overrides));const proxyNavigator=new Proxy(nav,{{get(target,prop,receiver){{if(typeof prop==='string'&&Object.prototype.hasOwnProperty.call(overrides,prop))return overrides[prop];const value=Reflect.get(target,prop,receiver);return typeof value==='function'?value.bind(target):value;}},has(target,prop){{return(typeof prop==='string'&&Object.prototype.hasOwnProperty.call(overrides,prop))||prop in target;}},ownKeys(target){{const keys=Reflect.ownKeys(target);for(const key of Reflect.ownKeys(overrides)){{if(!keys.includes(key))keys.push(key);}}return keys;}},getOwnPropertyDescriptor(target,prop){{if(typeof prop==='string'&&Object.prototype.hasOwnProperty.call(overrides,prop)){{return{{configurable:true,enumerable:true,writable:false,value:overrides[prop]}};}}return Reflect.getOwnPropertyDescriptor(target,prop);}}}});const installNavigatorProxy=(target)=>{{if(!target)return false;try{{const descriptor=Object.getOwnPropertyDescriptor(target,'navigator');if(descriptor&&descriptor.configurable===false)return false;Object.defineProperty(target,'navigator',{{configurable:true,get:()=>proxyNavigator}});return true;}}catch(_e){{return false;}}}};installNavigatorProxy(window);installNavigatorProxy(globalThis);if(window.Window&&window.Window.prototype)installNavigatorProxy(window.Window.prototype);for(const key of overriddenKeys){{try{{delete nav[key];}}catch(_e){{}}}} }})();",
      overrides.join(""),
      overrides_object
    ))
  }

  fn merge_geolocation_defaults(
    fingerprint: &mut Value,
    geo: &crate::camoufox::geolocation::Geolocation,
  ) {
    let has_timezone = Self::json_string(fingerprint, &["timezone", "timeZone"]).is_some();
    let has_language = Self::json_string(fingerprint, &["language", "locale"]).is_some();
    let has_languages = Self::json_languages(fingerprint).is_some();
    let has_latitude = fingerprint
      .get("latitude")
      .and_then(|value| value.as_f64())
      .is_some();
    let has_longitude = fingerprint
      .get("longitude")
      .and_then(|value| value.as_f64())
      .is_some();

    if let Some(obj) = fingerprint.as_object_mut() {
      if !has_timezone {
        obj.insert("timezone".to_string(), json!(geo.timezone));
      }

      let locale_str = geo.locale.as_string();
      if !has_language {
        obj.insert("language".to_string(), json!(&locale_str));
      }
      if !has_languages {
        obj.insert(
          "languages".to_string(),
          json!([&locale_str, &geo.locale.language]),
        );
      }
      if !has_latitude {
        obj.insert("latitude".to_string(), json!(geo.latitude));
      }
      if !has_longitude {
        obj.insert("longitude".to_string(), json!(geo.longitude));
      }
    }
  }

  async fn apply_runtime_fingerprint_overrides(
    &self,
    ws_url: &str,
    profile: &BrowserProfile,
    fingerprint: &Value,
  ) {
    let _ = self
      .send_cdp_command(ws_url, "Page.enable", json!({}))
      .await;
    let _ = self
      .send_cdp_command(ws_url, "Runtime.enable", json!({}))
      .await;

    if let Some(params) = Self::user_agent_override_params(profile, fingerprint) {
      if let Err(e) = self
        .send_cdp_command(ws_url, "Emulation.setUserAgentOverride", params)
        .await
      {
        log::warn!("Failed to apply user-agent override via CDP: {e}");
      }
    }

    if let Some(source) = Self::fingerprint_override_script(profile, fingerprint) {
      if let Err(e) = self
        .send_cdp_command(
          ws_url,
          "Page.addScriptToEvaluateOnNewDocument",
          json!({
            "source": source,
            "runImmediately": true
          }),
        )
        .await
      {
        log::warn!("Failed to inject fingerprint override script via CDP: {e}");
      }

      if let Err(e) = self
        .send_cdp_command(
          ws_url,
          "Runtime.evaluate",
          json!({
            "expression": source,
            "returnByValue": true,
          }),
        )
        .await
      {
        log::warn!("Failed to apply runtime fingerprint override in current page via CDP: {e}");
      }
    }
  }

  pub async fn refresh_runtime_fingerprint_overrides_for_target(
    &self,
    profile: &BrowserProfile,
    ws_url: &str,
  ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(config) = profile.chromium_config.as_ref() else {
      return Ok(());
    };
    let fingerprint = Self::stored_fingerprint_value(config);
    if fingerprint
      .as_object()
      .map(|value| value.is_empty())
      .unwrap_or(true)
    {
      return Ok(());
    }

    self
      .apply_runtime_fingerprint_overrides(ws_url, profile, &fingerprint)
      .await;
    Ok(())
  }

  fn fingerprint_chromium_launch_args(
    profile: &BrowserProfile,
    config: &ChromiumConfig,
  ) -> Vec<String> {
    let fingerprint = Self::stored_fingerprint_value(config);

    let mut args = Vec::new();

    args.push(format!(
      "--fingerprint={}",
      Self::fingerprint_seed(config, profile)
    ));

    let platform = config
      .os
      .clone()
      .or_else(|| profile.resolved_os().map(ToOwned::to_owned))
      .unwrap_or_else(|| Self::current_fingerprint_platform().to_string());
    args.push(format!("--fingerprint-platform={platform}"));

    if let Some(platform_version) = Self::json_string(
      &fingerprint,
      &["platformVersion", "osVersion", "platform_version"],
    ) {
      args.push(format!("--fingerprint-platform-version={platform_version}"));
    }

    let brand = Self::json_string(&fingerprint, &["browserBrand", "brand", "fingerprintBrand"])
      .unwrap_or_else(|| "Chrome".to_string());
    args.push(format!("--fingerprint-brand={brand}"));

    let brand_version = Self::json_string(
      &fingerprint,
      &[
        "browserVersion",
        "brandVersion",
        "fingerprintBrandVersion",
        "chromeVersion",
      ],
    )
    .map(|_| Self::runtime_browser_major_version(profile))
    .unwrap_or_else(|| "142".to_string());
    args.push(format!("--fingerprint-brand-version={brand_version}"));

    let hardware_concurrency = Self::json_u32(
      &fingerprint,
      &[
        "hardwareConcurrency",
        "hardware_concurrency",
        "cpuCores",
        "cpu_cores",
      ],
    )
    .unwrap_or(8);
    args.push(format!(
      "--fingerprint-hardware-concurrency={hardware_concurrency}"
    ));

    let language = Self::json_string(&fingerprint, &["language", "locale"])
      .or_else(|| {
        Self::json_languages(&fingerprint)
          .and_then(|langs| langs.split(',').next().map(str::to_string))
      })
      .unwrap_or_else(|| "en-US".to_string());
    args.push(format!("--lang={language}"));

    let accept_language =
      Self::json_languages(&fingerprint).unwrap_or_else(|| format!("{language},en-US,en"));
    args.push(format!("--accept-lang={accept_language}"));

    let timezone = Self::json_string(&fingerprint, &["timezone", "timeZone"])
      .unwrap_or_else(|| "America/New_York".to_string());
    args.push(format!("--timezone={timezone}"));

    if let Some(vendor) = Self::json_string(
      &fingerprint,
      &["webglVendor", "webgl_vendor", "gpuVendor", "gpu_vendor"],
    ) {
      args.push(format!("--fingerprint-gpu-vendor={vendor}"));
    }

    if let Some(renderer) = Self::json_string(
      &fingerprint,
      &[
        "webglRenderer",
        "webgl_renderer",
        "gpuRenderer",
        "gpu_renderer",
      ],
    ) {
      args.push(format!("--fingerprint-gpu-renderer={renderer}"));
    }

    args
  }

  async fn wait_for_cdp_ready(
    &self,
    port: u16,
  ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("http://127.0.0.1:{port}/json/version");
    // On first launch, macOS Gatekeeper verifies the binary which can take 30+ seconds.
    // Use a generous timeout (60s) to handle this.
    let max_attempts = 120;
    let delay = Duration::from_millis(500);

    let mut last_error: Option<String> = None;
    for attempt in 0..max_attempts {
      match self.http_client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
          log::info!("CDP ready on port {port} after {attempt} attempts");
          return Ok(());
        }
        Ok(resp) => {
          last_error = Some(format!("HTTP {} from {url}", resp.status()));
          tokio::time::sleep(delay).await;
        }
        Err(e) => {
          last_error = Some(format!("request failed: {e}"));
          tokio::time::sleep(delay).await;
        }
      }
    }

    let detail = last_error.unwrap_or_else(|| "no attempts completed".to_string());
    // Log at error level so we can diagnose Windows/AV/firewall-induced CDP hangs
    // in customer reports without needing them to reproduce in the moment.
    log::error!("CDP not ready after {max_attempts} attempts on port {port}: {detail}");
    Err(format!("CDP not ready after {max_attempts} attempts on port {port}: {detail}").into())
  }

  async fn get_cdp_targets(
    &self,
    port: u16,
  ) -> Result<Vec<CdpTarget>, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("http://127.0.0.1:{port}/json");
    let resp = self.http_client.get(&url).send().await?;
    let targets: Vec<CdpTarget> = resp.json().await?;
    Ok(targets)
  }

  async fn send_cdp_command(
    &self,
    ws_url: &str,
    method: &str,
    params: serde_json::Value,
  ) -> Result<serde_json::Value, Box<dyn std::error::Error + Send + Sync>> {
    let (mut ws_stream, _) = connect_async(ws_url).await?;

    let command = json!({
      "id": 1,
      "method": method,
      "params": params
    });

    use futures_util::sink::SinkExt;
    use futures_util::stream::StreamExt;

    ws_stream
      .send(Message::Text(command.to_string().into()))
      .await?;

    while let Some(msg) = ws_stream.next().await {
      match msg? {
        Message::Text(text) => {
          let response: serde_json::Value = serde_json::from_str(text.as_str())?;
          if response.get("id") == Some(&json!(1)) {
            if let Some(error) = response.get("error") {
              return Err(format!("CDP error: {}", error).into());
            }
            return Ok(response.get("result").cloned().unwrap_or(json!({})));
          }
        }
        Message::Close(_) => break,
        _ => {}
      }
    }

    Err("No response received from CDP".into())
  }

  pub async fn generate_fingerprint_config(
    &self,
    _app_handle: &AppHandle,
    profile: &BrowserProfile,
    config: &ChromiumConfig,
  ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    // fingerprint-chromium does not expose the proprietary legacy Chromium CDP
    // fingerprint generation methods. Generate a stable, minimal fingerprint
    // document for our existing config storage, then map it to command-line
    // arguments during launch.
    let os = config
      .os
      .clone()
      .or_else(|| profile.resolved_os().map(ToOwned::to_owned))
      .unwrap_or_else(|| Self::current_fingerprint_platform().to_string());

    let seed = Self::resolved_seed(profile, config);
    let mut fingerprint = json!({
      "seed": seed,
      "platform": os,
      "browserBrand": "Chrome",
      "browserVersion": "142",
      "hardwareConcurrency": 8,
      "language": "en-US",
      "languages": ["en-US", "en"],
      "timezone": "America/New_York"
    });

    if let Some(existing) = config
      .fingerprint
      .as_deref()
      .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
    {
      let existing = existing.get("fingerprint").cloned().unwrap_or(existing);
      if let Some(target) = fingerprint.as_object_mut() {
        let source = if config.randomize_fingerprint_on_launch == Some(true) {
          Self::preserved_fixed_fingerprint_fields(&existing)
        } else {
          existing.as_object().cloned().unwrap_or_default()
        };
        for (key, value) in source {
          target.insert(key.clone(), value.clone());
        }
      }
    }

    // Apply geolocation defaults when requested. This mirrors the old behavior
    // without relying on proprietary legacy CDP methods.
    let geoip_option = config.geoip.as_ref();
    let should_geolocate = !matches!(geoip_option, Some(serde_json::Value::Bool(false)));

    if should_geolocate {
      let geo_result = async {
        let ip = match geoip_option {
          Some(serde_json::Value::String(ip_str)) => ip_str.clone(),
          _ => crate::ip_utils::fetch_public_ip(config.proxy.as_deref())
            .await
            .map_err(|e| format!("Failed to fetch public IP: {e}"))?,
        };

        crate::camoufox::geolocation::get_geolocation(&ip)
          .map_err(|e| format!("Failed to get geolocation for IP {ip}: {e}"))
      }
      .await;

      match geo_result {
        Ok(geo) => {
          Self::merge_geolocation_defaults(&mut fingerprint, &geo);
        }
        Err(e) => {
          log::warn!("fingerprint-chromium geolocation failed, using existing/default values: {e}");
        }
      }
    }

    let fingerprint_json = serde_json::to_string(&fingerprint)
      .map_err(|e| format!("Failed to serialize fingerprint: {e}"))?;

    log::info!(
      "Generated fingerprint-chromium config for OS: {}, fields: {:?}",
      os,
      fingerprint
        .as_object()
        .map(|o| o.keys().collect::<Vec<_>>())
    );

    Ok(fingerprint_json)
  }

  #[allow(clippy::too_many_arguments)]
  pub async fn launch_chromium(
    &self,
    _app_handle: &AppHandle,
    profile: &BrowserProfile,
    profile_path: &str,
    config: &ChromiumConfig,
    url: Option<&str>,
    proxy_url: Option<&str>,
    ephemeral: bool,
    extension_paths: &[String],
    remote_debugging_port: Option<u16>,
    headless: bool,
  ) -> Result<ChromiumLaunchResult, Box<dyn std::error::Error + Send + Sync>> {
    let executable_path = BrowserRunner::instance()
      .get_browser_executable_path(profile)
      .map_err(|e| format!("Failed to get Chromium executable path: {e}"))?;

    let port = match remote_debugging_port {
      Some(p) => p,
      None => Self::find_free_port().await?,
    };
    log::info!("Launching Chromium on CDP port {port} (detached)");

    // Diagnostic: verify critical profile files and test cookie decryption
    {
      let profile_path_buf = std::path::PathBuf::from(profile_path);
      let key_path = profile_path_buf.join("os_crypt_key");
      let cookies_path = {
        let network = profile_path_buf
          .join("Default")
          .join("Network")
          .join("Cookies");
        if network.exists() {
          network
        } else {
          profile_path_buf.join("Default").join("Cookies")
        }
      };

      if key_path.exists() {
        let key_text = std::fs::read_to_string(&key_path).unwrap_or_default();
        log::info!(
          "Pre-launch: os_crypt_key present ({} bytes, content: '{}')",
          key_text.len(),
          key_text.trim()
        );
      } else {
        log::warn!("Pre-launch: os_crypt_key NOT FOUND");
      }

      if cookies_path.exists() {
        // Try to open Cookies DB and check if encrypted cookies can be decrypted
        if let Ok(conn) = rusqlite::Connection::open_with_flags(
          &cookies_path,
          rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) {
          let cookie_count: i64 = conn
            .query_row(
              "SELECT COUNT(*) FROM cookies WHERE length(encrypted_value) > 0",
              [],
              |r| r.get(0),
            )
            .unwrap_or(0);
          let total_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM cookies", [], |r| r.get(0))
            .unwrap_or(0);
          log::info!(
            "Pre-launch: Cookies DB has {} total cookies, {} encrypted",
            total_count,
            cookie_count
          );

          // Try decrypting one cookie using the cookie_manager
          if let Some(encryption_key) =
            crate::cookie_manager::chrome_decrypt::get_encryption_key(&profile_path_buf)
          {
            if let Ok(mut stmt) = conn.prepare(
              "SELECT name, host_key, encrypted_value FROM cookies WHERE length(encrypted_value) > 0 LIMIT 1",
            ) {
              if let Ok(mut rows) = stmt.query([]) {
                if let Ok(Some(row)) = rows.next() {
                  let name: String = row.get(0).unwrap_or_default();
                  let host: String = row.get(1).unwrap_or_default();
                  let encrypted: Vec<u8> = row.get(2).unwrap_or_default();
                  let decrypted = crate::cookie_manager::chrome_decrypt::decrypt(
                    &encrypted,
                    &host,
                    &encryption_key,
                  );
                  match decrypted {
                    Some(val) => log::info!(
                      "Pre-launch: Cookie decryption SUCCEEDED for '{}' (host: {}, decrypted {} bytes)",
                      name, host, val.len()
                    ),
                    None => log::error!(
                      "Pre-launch: Cookie decryption FAILED for '{}' (host: {}, encrypted {} bytes)",
                      name, host, encrypted.len()
                    ),
                  }
                }
              }
            }
          } else {
            log::error!("Pre-launch: Failed to derive encryption key from os_crypt_key");
          }
        }
      } else {
        log::warn!("Pre-launch: Cookies NOT FOUND");
      }
    }

    let fingerprint = Self::stored_fingerprint_value(config);

    let mut args = vec![
      format!("--remote-debugging-port={port}"),
      "--remote-debugging-address=127.0.0.1".to_string(),
      format!("--user-data-dir={profile_path}"),
      "--no-first-run".to_string(),
      "--no-default-browser-check".to_string(),
      "--disable-background-mode".to_string(),
      "--disable-component-update".to_string(),
      "--disable-background-timer-throttling".to_string(),
      "--crash-server-url=".to_string(),
      "--disable-updater".to_string(),
      "--disable-session-crashed-bubble".to_string(),
      "--hide-crash-restore-bubble".to_string(),
      "--disable-infobars".to_string(),
      "--disable-features=DialMediaRouteProvider,DnsOverHttps,AsyncDns".to_string(),
      "--use-mock-keychain".to_string(),
      "--password-store=basic".to_string(),
      "--disable-non-proxied-udp".to_string(),
    ];

    args.extend(Self::fingerprint_chromium_launch_args(profile, config));

    if headless {
      args.push("--headless=new".to_string());
    }

    #[cfg(target_os = "linux")]
    {
      args.push("--no-sandbox".to_string());
      args.push("--disable-setuid-sandbox".to_string());
      args.push("--disable-dev-shm-usage".to_string());
    }

    if ephemeral {
      args.push("--disk-cache-size=1".to_string());
      args.push("--disable-breakpad".to_string());
      args.push("--disable-crash-reporter".to_string());
      args.push("--no-service-autorun".to_string());
      args.push("--disable-sync".to_string());
    }

    args.extend(Self::chromium_extension_launch_args(extension_paths));

    if let Some(proxy) = proxy_url {
      args.push(format!("--proxy-server={proxy}"));
      args.push("--dns-prefetch-disable".to_string());
    }

    let mut command = TokioCommand::new(&executable_path);
    command
      .args(&args)
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null());

    let child = command
      .spawn()
      .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
        let hint = if e.raw_os_error() == Some(14001) {
          ". This usually means the Visual C++ Redistributable is not installed. \
           Download it from https://aka.ms/vs/17/release/vc_redist.x64.exe"
        } else {
          ""
        };
        format!("Failed to spawn Chromium: {e}{hint}").into()
      })?;
    let process_id = child.id();
    drop(child);

    self.wait_for_cdp_ready(port).await?;

    let targets = self.get_cdp_targets(port).await?;
    log::info!("Found {} CDP targets", targets.len());

    let page_targets: Vec<_> = targets.iter().filter(|t| t.target_type == "page").collect();
    log::info!("Found {} page targets", page_targets.len());

    for target in &page_targets {
      if let Some(ws_url) = &target.websocket_debugger_url {
        self
          .apply_runtime_fingerprint_overrides(ws_url, profile, &fingerprint)
          .await;
      }
    }

    // fingerprint-chromium consumes fingerprint data through command-line
    // arguments. Do not call legacy proprietary CDP methods and do not inject
    // any retired cloud token into the browser process.
    if let Some(fingerprint_json) = &config.fingerprint {
      log::info!(
        "fingerprint-chromium launch args were derived from stored fingerprint ({} chars)",
        fingerprint_json.len()
      );
    } else {
      log::warn!("No fingerprint found in config, fingerprint-chromium will use seed defaults");
    }

    // Geolocation is handled internally by the browser binary.

    if let Some(url) = url {
      log::info!("Navigating to URL via CDP: {}", url);
      if let Some(target) = page_targets.first() {
        if let Some(ws_url) = &target.websocket_debugger_url {
          if let Err(e) = self
            .send_cdp_command(ws_url, "Page.navigate", json!({ "url": url }))
            .await
          {
            log::error!("Failed to navigate to URL: {e}");
          }
        }
      }
    }

    for target in &page_targets {
      if let Some(ws_url) = &target.websocket_debugger_url {
        let _ = self
          .send_cdp_command(ws_url, "Emulation.clearDeviceMetricsOverride", json!({}))
          .await;
        let _ = self
          .send_cdp_command(
            ws_url,
            "Emulation.setFocusEmulationEnabled",
            json!({ "enabled": false }),
          )
          .await;
        let _ = self
          .send_cdp_command(
            ws_url,
            "Emulation.setEmulatedMedia",
            json!({ "media": "", "features": [] }),
          )
          .await;
      }
    }

    let id = uuid::Uuid::new_v4().to_string();
    let instance = ChromiumInstance {
      id: id.clone(),
      process_id,
      profile_path: Some(profile_path.to_string()),
      url: url.map(|s| s.to_string()),
      cdp_port: Some(port),
    };

    let mut inner = self.inner.lock().await;
    inner.instances.insert(id.clone(), instance);

    Ok(ChromiumLaunchResult {
      id,
      processId: process_id,
      profilePath: Some(profile_path.to_string()),
      url: url.map(|s| s.to_string()),
      cdp_port: Some(port),
    })
  }

  pub async fn stop_chromium(
    &self,
    id: &str,
  ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut inner = self.inner.lock().await;

    if let Some(instance) = inner.instances.remove(id) {
      log::info!("Cleaning up Chromium instance {}", instance.id);
      if let Some(pid) = instance.process_id {
        #[cfg(unix)]
        {
          use nix::sys::signal::{kill, Signal};
          use nix::unistd::Pid;
          let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
        }
        #[cfg(windows)]
        {
          use std::os::windows::process::CommandExt;
          const CREATE_NO_WINDOW: u32 = 0x08000000;
          let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        }
        log::info!("Stopped Chromium instance {id} (PID: {pid})");
      }
    }

    Ok(())
  }

  /// Opens a URL in a new tab for an existing Chromium instance.
  pub async fn open_url_in_tab(
    &self,
    profile_path: &str,
    url: &str,
  ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let inner = self.inner.lock().await;
    let target_path = std::path::Path::new(profile_path)
      .canonicalize()
      .unwrap_or_else(|_| std::path::Path::new(profile_path).to_path_buf());

    let port = inner
      .instances
      .values()
      .find(|i| {
        i.profile_path
          .as_deref()
          .map(|p| {
            std::path::Path::new(p)
              .canonicalize()
              .unwrap_or_else(|_| std::path::Path::new(p).to_path_buf())
              == target_path
          })
          .unwrap_or(false)
      })
      .and_then(|i| i.cdp_port)
      .ok_or("Chromium instance (with CDP port) not found for profile")?;
    drop(inner);

    // Open the URL in a new tab via the CDP HTTP convenience endpoint.
    let new_tab_url = format!(
      "http://127.0.0.1:{port}/json/new?{}",
      urlencoding::encode(url)
    );
    let resp = self
      .http_client
      .put(&new_tab_url)
      .send()
      .await
      .map_err(|e| format!("Failed to open new tab: {e}"))?;
    if !resp.status().is_success() {
      return Err(format!("CDP /json/new returned HTTP {}", resp.status()).into());
    }

    log::info!("Opened URL in new tab via CDP: {}", url);
    Ok(())
  }

  pub async fn get_cdp_port(&self, profile_path: &str) -> Option<u16> {
    let inner = self.inner.lock().await;
    let target_path = std::path::Path::new(profile_path)
      .canonicalize()
      .unwrap_or_else(|_| std::path::Path::new(profile_path).to_path_buf());

    for instance in inner.instances.values() {
      if let Some(path) = &instance.profile_path {
        let instance_path = std::path::Path::new(path)
          .canonicalize()
          .unwrap_or_else(|_| std::path::Path::new(path).to_path_buf());
        if instance_path == target_path {
          return instance.cdp_port;
        }
      }
    }
    None
  }

  pub async fn find_chromium_by_profile(&self, profile_path: &str) -> Option<ChromiumLaunchResult> {
    use sysinfo::{ProcessRefreshKind, RefreshKind, System};

    let mut inner = self.inner.lock().await;

    // Canonicalize the target path for comparison
    let target_path = std::path::Path::new(profile_path)
      .canonicalize()
      .unwrap_or_else(|_| std::path::Path::new(profile_path).to_path_buf());

    // Find the instance with the matching profile path
    let mut found_id: Option<String> = None;
    for (id, instance) in &inner.instances {
      if let Some(path) = &instance.profile_path {
        let instance_path = std::path::Path::new(path)
          .canonicalize()
          .unwrap_or_else(|_| std::path::Path::new(path).to_path_buf());
        if instance_path == target_path {
          found_id = Some(id.clone());
          break;
        }
      }
    }

    // If we found an instance, verify the process is still running
    if let Some(id) = found_id {
      if let Some(instance) = inner.instances.get(&id) {
        if let Some(pid) = instance.process_id {
          let system = System::new_with_specifics(
            RefreshKind::nothing().with_processes(ProcessRefreshKind::everything()),
          );
          let sysinfo_pid = sysinfo::Pid::from_u32(pid);

          if system.process(sysinfo_pid).is_some() {
            return Some(ChromiumLaunchResult {
              id: id.clone(),
              processId: instance.process_id,
              profilePath: instance.profile_path.clone(),
              url: instance.url.clone(),
              cdp_port: instance.cdp_port,
            });
          } else {
            log::info!(
              "Chromium process {} for profile {} is no longer running, cleaning up",
              pid,
              profile_path
            );
            inner.instances.remove(&id);
            return None;
          }
        }
      }
    }

    // If not found in in-memory instances, scan system processes.
    // This handles the case where the GUI was restarted but Chromium is still running.
    if let Some((pid, found_profile_path, cdp_port)) =
      Self::find_chromium_process_by_profile(&target_path)
    {
      log::info!(
        "Found running Chromium process (PID: {}) for profile path via system scan",
        pid
      );

      let instance_id = format!("recovered_{}", pid);
      inner.instances.insert(
        instance_id.clone(),
        ChromiumInstance {
          id: instance_id.clone(),
          process_id: Some(pid),
          profile_path: Some(found_profile_path.clone()),
          url: None,
          cdp_port,
        },
      );

      return Some(ChromiumLaunchResult {
        id: instance_id,
        processId: Some(pid),
        profilePath: Some(found_profile_path),
        url: None,
        cdp_port,
      });
    }

    None
  }

  /// Scan system processes to find a Chromium process using a specific profile path
  fn find_chromium_process_by_profile(
    target_path: &std::path::Path,
  ) -> Option<(u32, String, Option<u16>)> {
    use sysinfo::{ProcessRefreshKind, RefreshKind, System};

    let system = System::new_with_specifics(
      RefreshKind::nothing().with_processes(ProcessRefreshKind::everything()),
    );

    let target_path_str = target_path.to_string_lossy();

    for (pid, process) in system.processes() {
      let cmd = process.cmd();
      if cmd.is_empty() {
        continue;
      }

      let exe_name = process.name().to_string_lossy();
      let is_chromium_like = crate::browser::chromium_process_name_looks_like(&exe_name);

      if !is_chromium_like {
        continue;
      }

      // Skip child processes (renderer, GPU, utility, zygote, etc.)
      // Only the main browser process lacks a --type= argument
      let is_child = cmd
        .iter()
        .any(|a| a.to_str().is_some_and(|s| s.starts_with("--type=")));
      if is_child {
        continue;
      }

      let mut matched = false;
      let mut cdp_port: Option<u16> = None;

      for arg in cmd.iter() {
        if let Some(arg_str) = arg.to_str() {
          if let Some(dir_val) = arg_str.strip_prefix("--user-data-dir=") {
            let cmd_path = std::path::Path::new(dir_val)
              .canonicalize()
              .unwrap_or_else(|_| std::path::Path::new(dir_val).to_path_buf());
            if cmd_path == target_path {
              matched = true;
            }
          }

          if let Some(port_val) = arg_str.strip_prefix("--remote-debugging-port=") {
            cdp_port = port_val.parse().ok();
          }
        }
      }

      if matched {
        return Some((pid.as_u32(), target_path_str.to_string(), cdp_port));
      }
    }

    None
  }

  #[allow(dead_code)]
  pub async fn launch_chromium_profile(
    &self,
    app_handle: &AppHandle,
    profile: &BrowserProfile,
    config: &ChromiumConfig,
    url: Option<&str>,
    proxy_url: Option<&str>,
  ) -> Result<ChromiumLaunchResult, Box<dyn std::error::Error + Send + Sync>> {
    let profiles_dir = self.get_profiles_dir();
    let profile_path = profiles_dir.join(profile.id.to_string()).join("profile");
    let profile_path_str = profile_path.to_string_lossy().to_string();

    std::fs::create_dir_all(&profile_path)?;

    if let Some(existing) = self.find_chromium_by_profile(&profile_path_str).await {
      log::info!("Stopping existing Chromium instance for profile");
      self.stop_chromium(&existing.id).await?;
    }

    self
      .launch_chromium(
        app_handle,
        profile,
        &profile_path_str,
        config,
        url,
        proxy_url,
        profile.ephemeral,
        &[],
        None,
        false,
      )
      .await
  }

  #[allow(dead_code)]
  pub async fn cleanup_dead_instances(&self) {
    use sysinfo::{ProcessRefreshKind, RefreshKind, System};

    let mut inner = self.inner.lock().await;
    let mut dead_ids = Vec::new();

    let system = System::new_with_specifics(
      RefreshKind::nothing().with_processes(ProcessRefreshKind::everything()),
    );

    for (id, instance) in &inner.instances {
      if let Some(pid) = instance.process_id {
        let pid = sysinfo::Pid::from_u32(pid);
        if !system.processes().contains_key(&pid) {
          dead_ids.push(id.clone());
        }
      }
    }

    for id in dead_ids {
      log::info!("Cleaning up dead Chromium instance: {id}");
      inner.instances.remove(&id);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::ChromiumManager;
  use crate::profile::BrowserProfile;
  use serde_json::json;

  #[test]
  fn test_chromium_extension_launch_args_empty() {
    let args = ChromiumManager::chromium_extension_launch_args(&[]);
    assert!(args.is_empty());
  }

  #[test]
  fn test_chromium_extension_launch_args_includes_load_and_disable_except() {
    let args = ChromiumManager::chromium_extension_launch_args(&[
      "/tmp/ext-a".to_string(),
      "/tmp/ext-b".to_string(),
    ]);
    assert_eq!(
      args,
      vec![
        "--load-extension=/tmp/ext-a,/tmp/ext-b".to_string(),
        "--disable-extensions-except=/tmp/ext-a,/tmp/ext-b".to_string(),
      ]
    );
  }

  fn test_profile() -> BrowserProfile {
    BrowserProfile {
      id: uuid::Uuid::new_v4(),
      name: "test".to_string(),
      browser: "chromium".to_string(),
      version: "142.0.7444.175".to_string(),
      ..BrowserProfile::default()
    }
  }

  #[test]
  fn test_normalize_user_agent_for_runtime_rewrites_legacy_major_version() {
    let profile = test_profile();
    let raw = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36";
    let normalized = ChromiumManager::normalize_user_agent_for_runtime(raw, &profile);
    assert!(normalized.contains("Chrome/142.0.0.0"));
    assert!(!normalized.contains("Chrome/146.0.0.0"));
  }

  #[test]
  fn test_user_agent_override_params_include_accept_language_and_platform() {
    let profile = test_profile();
    let params = ChromiumManager::user_agent_override_params(
      &profile,
      &json!({
        "userAgent": "Mozilla/5.0 Chrome/146.0.0.0 Safari/537.36",
        "languages": ["zh-HK", "zh"],
        "platform": "MacIntel"
      }),
    )
    .expect("override params should exist");

    assert_eq!(
      params["userAgent"],
      "Mozilla/5.0 Chrome/142.0.0.0 Safari/537.36"
    );
    assert_eq!(params["acceptLanguage"], "zh-HK,zh");
    assert_eq!(params["platform"], "MacIntel");
  }

  #[test]
  fn test_fingerprint_override_script_injects_device_memory() {
    let script = ChromiumManager::fingerprint_override_script(
      &test_profile(),
      &json!({
        "deviceMemory": 16,
        "userAgent": "Mozilla/5.0 Chrome/146.0.0.0 Safari/537.36",
        "languages": ["zh-HK", "zh"]
      }),
    )
    .expect("script should be generated");

    assert!(script.contains("deviceMemory"));
    assert!(script.contains("16.0") || script.contains("16"));
    assert!(script.contains("userAgent"));
    assert!(script.contains("zh-HK"));
    assert!(script.contains("proxyNavigator"));
    assert!(script.contains("installNavigatorProxy"));
  }

  #[test]
  fn test_fingerprint_launch_args_use_stored_seed() {
    let profile = test_profile();
    let config = super::ChromiumConfig {
      fingerprint: Some(
        json!({
          "seed": 123456789u32,
          "platform": "macos"
        })
        .to_string(),
      ),
      ..Default::default()
    };

    let args = ChromiumManager::fingerprint_chromium_launch_args(&profile, &config);

    assert!(args.contains(&"--fingerprint=123456789".to_string()));
  }

  #[test]
  fn test_fingerprint_launch_args_fall_back_to_stable_seed() {
    let profile = test_profile();
    let config = super::ChromiumConfig::default();

    let args = ChromiumManager::fingerprint_chromium_launch_args(&profile, &config);
    let expected = format!(
      "--fingerprint={}",
      ChromiumManager::stable_fingerprint_seed(&profile)
    );

    assert!(args.contains(&expected));
  }

  #[test]
  fn test_merge_geolocation_defaults_preserves_fixed_locale_values() {
    let mut fingerprint = json!({
      "language": "fr-FR",
      "languages": ["fr-FR", "fr"],
      "timezone": "Europe/Paris",
      "latitude": 48.8566,
      "longitude": 2.3522
    });
    let geo = crate::camoufox::geolocation::Geolocation {
      locale: crate::camoufox::geolocation::Locale {
        language: "zh".to_string(),
        region: Some("CN".to_string()),
        script: None,
      },
      longitude: 121.4737,
      latitude: 31.2304,
      timezone: "Asia/Shanghai".to_string(),
      accuracy: None,
    };

    ChromiumManager::merge_geolocation_defaults(&mut fingerprint, &geo);

    assert_eq!(fingerprint["language"], "fr-FR");
    assert_eq!(fingerprint["languages"], json!(["fr-FR", "fr"]));
    assert_eq!(fingerprint["timezone"], "Europe/Paris");
    assert_eq!(fingerprint["latitude"], 48.8566);
    assert_eq!(fingerprint["longitude"], 2.3522);
  }
}

lazy_static::lazy_static! {
  static ref CHROMIUM_MANAGER: ChromiumManager = ChromiumManager::new();
}

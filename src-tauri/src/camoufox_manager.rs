use crate::browser_runner::BrowserRunner;
use crate::camoufox::launcher::{CamoufoxLauncher, LaunchOptions};
use crate::camoufox::{CamoufoxConfigBuilder, GeoIPOption, ScreenConstraints};
use crate::profile::BrowserProfile;
use playwright::api::Viewport;
use playwright::api::{BrowserContext, Page};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::AppHandle;
use tokio::sync::Mutex as AsyncMutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CamoufoxConfig {
  pub proxy: Option<String>,
  pub screen_max_width: Option<u32>,
  pub screen_max_height: Option<u32>,
  pub screen_min_width: Option<u32>,
  pub screen_min_height: Option<u32>,
  pub geoip: Option<serde_json::Value>, // Can be String or bool
  pub block_images: Option<bool>,
  pub block_webrtc: Option<bool>,
  pub block_webgl: Option<bool>,
  pub fingerprint: Option<String>, // JSON string of the complete fingerprint config
  pub randomize_fingerprint_on_launch: Option<bool>, // Generate new fingerprint on every launch
  pub os: Option<String>, // Operating system for fingerprint generation: "windows", "macos", or "linux"
}

impl Default for CamoufoxConfig {
  fn default() -> Self {
    Self {
      proxy: None,
      screen_max_width: None,
      screen_max_height: None,
      screen_min_width: None,
      screen_min_height: None,
      geoip: Some(serde_json::Value::Bool(true)),
      block_images: None,
      block_webrtc: None,
      block_webgl: None,
      fingerprint: None,
      randomize_fingerprint_on_launch: None,
      os: None,
    }
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct CamoufoxLaunchResult {
  pub id: String,
  #[serde(alias = "process_id")]
  pub processId: Option<u32>,
  #[serde(alias = "profile_path")]
  pub profilePath: Option<String>,
  pub url: Option<String>,
  pub cdp_port: Option<u16>,
}

#[derive(Debug)]
struct CamoufoxAutomationState {
  active_page: Page,
  selected_tab_index: usize,
}

#[derive(Debug)]
struct CamoufoxInstance {
  #[allow(dead_code)]
  id: String,
  process_id: Option<u32>,
  profile_path: Option<String>,
  url: Option<String>,
  cdp_port: Option<u16>,
  automation: Option<Arc<AsyncMutex<CamoufoxAutomationState>>>,
  #[allow(dead_code)]
  launcher: Option<Arc<CamoufoxLauncher>>,
}

struct CamoufoxManagerInner {
  instances: HashMap<String, CamoufoxInstance>,
}

pub struct CamoufoxManager {
  inner: Arc<AsyncMutex<CamoufoxManagerInner>>,
}

impl CamoufoxManager {
  fn config_string(config: &HashMap<String, serde_json::Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
      config
        .get(*key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
    })
  }

  fn config_u32(config: &HashMap<String, serde_json::Value>, keys: &[&str]) -> Option<u32> {
    keys.iter().find_map(|key| {
      config
        .get(*key)
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok())
    })
  }

  fn config_languages(config: &HashMap<String, serde_json::Value>) -> Option<Vec<String>> {
    if let Some(values) = config
      .get("navigator.languages")
      .and_then(|value| value.as_array())
    {
      let parsed = values
        .iter()
        .filter_map(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
      if !parsed.is_empty() {
        return Some(parsed);
      }
    }

    let language = Self::config_string(config, &["navigator.language", "locale:language"])?;
    let region = Self::config_string(config, &["locale:region"]);
    let primary = match region {
      Some(region) => format!("{}-{}", language, region.to_uppercase()),
      None => language.clone(),
    };

    let mut languages = vec![primary];
    if !languages.iter().any(|value| value == &language) {
      languages.push(language);
    }
    Some(languages)
  }

  fn runtime_override_script(config: &HashMap<String, serde_json::Value>) -> Option<String> {
    let mut entries = Vec::new();

    if let Some(user_agent) = Self::config_string(config, &["navigator.userAgent"]) {
      let serialized = serde_json::to_string(&user_agent).ok()?;
      entries.push(("userAgent".to_string(), serialized));
    }

    if let Some(platform) = Self::config_string(config, &["navigator.platform"]) {
      let serialized = serde_json::to_string(&platform).ok()?;
      entries.push(("platform".to_string(), serialized));
    }

    if let Some(language) = Self::config_string(config, &["navigator.language"])
      .or_else(|| Self::config_languages(config).and_then(|languages| languages.into_iter().next()))
    {
      let serialized = serde_json::to_string(&language).ok()?;
      entries.push(("language".to_string(), serialized));
    }

    if let Some(languages) = Self::config_languages(config) {
      let serialized = serde_json::to_string(&languages).ok()?;
      entries.push(("languages".to_string(), serialized));
    }

    if let Some(hardware_concurrency) = Self::config_u32(
      config,
      &["navigator.hardwareConcurrency", "hardwareConcurrency"],
    ) {
      let serialized = serde_json::to_string(&hardware_concurrency).ok()?;
      entries.push(("hardwareConcurrency".to_string(), serialized));
    }

    if let Some(timezone) = Self::config_string(config, &["timezone"]) {
      let serialized = serde_json::to_string(&timezone).ok()?;
      entries.push(("timezone".to_string(), serialized));
    }

    // Build matchMedia overrides from media config keys
    let match_media_js = Self::build_match_media_override(config);

    if entries.is_empty() && match_media_js.is_none() {
      return None;
    }

    let overrides_object = entries
      .iter()
      .map(|(key, value)| {
        serde_json::to_string(key)
          .ok()
          .map(|serialized_key| format!("{serialized_key}:{value}"))
      })
      .collect::<Option<Vec<_>>>()?
      .join(",");

    let match_media_block = match_media_js.unwrap_or_default();

    Some(format!(
      r#"(function(){{const nav=window.navigator;const proto=Object.getPrototypeOf(nav);const overrides={{{overrides_object}}};const define=(target,key,getter)=>{{if(!target)return false;try{{Object.defineProperty(target,key,{{configurable:true,get:getter}});return true;}}catch(_e){{return false;}}}};const overrideValue=(key,value)=>{{const getter=()=>value;define(nav,key,getter);define(proto,key,getter);}};for(const [key,value] of Object.entries(overrides)){{if(key!=="timezone"){{overrideValue(key,value);}}}}const proxyNavigator=new Proxy(nav,{{get(target,prop,receiver){{if(typeof prop==='string'&&Object.prototype.hasOwnProperty.call(overrides,prop)&&prop!=="timezone")return overrides[prop];const value=Reflect.get(target,prop,receiver);return typeof value==='function'?value.bind(target):value;}},has(target,prop){{return(typeof prop==='string'&&Object.prototype.hasOwnProperty.call(overrides,prop)&&prop!=="timezone")||prop in target;}},ownKeys(target){{const keys=Reflect.ownKeys(target);for(const key of Reflect.ownKeys(overrides)){{if(key!=="timezone"&&!keys.includes(key))keys.push(key);}}return keys;}},getOwnPropertyDescriptor(target,prop){{if(typeof prop==='string'&&Object.prototype.hasOwnProperty.call(overrides,prop)&&prop!=="timezone"){{return{{configurable:true,enumerable:true,writable:false,value:overrides[prop]}};}}return Reflect.getOwnPropertyDescriptor(target,prop);}}}});const installNavigatorProxy=(target)=>{{if(!target)return false;try{{const descriptor=Object.getOwnPropertyDescriptor(target,'navigator');if(descriptor&&descriptor.configurable===false)return false;Object.defineProperty(target,'navigator',{{configurable:true,get:()=>proxyNavigator}});return true;}}catch(_e){{return false;}}}};installNavigatorProxy(window);installNavigatorProxy(globalThis);if(window.Window&&window.Window.prototype)installNavigatorProxy(window.Window.prototype);if(typeof overrides.timezone==='string'&&overrides.timezone){{const OriginalDateTimeFormat=Intl.DateTimeFormat;const originalResolvedOptions=OriginalDateTimeFormat.prototype.resolvedOptions;Object.defineProperty(OriginalDateTimeFormat.prototype,'resolvedOptions',{{configurable:true,writable:true,value:function(...args){{const options=originalResolvedOptions.apply(this,args);return Object.assign({{}},options,{{timeZone:overrides.timezone}});}}}});Object.defineProperty(Intl,'DateTimeFormat',{{configurable:true,writable:true,value:function(...args){{return new OriginalDateTimeFormat(...args);}}}});Intl.DateTimeFormat.prototype=OriginalDateTimeFormat.prototype;}}{match_media_block};return true;}})()"#
    ))
  }

  /// Build a `matchMedia` override JS block from media query config keys.
  fn build_match_media_override(config: &HashMap<String, serde_json::Value>) -> Option<String> {
    // Map of config key → CSS media query feature name
    let media_mappings: &[(&str, &str)] = &[
      ("media:prefersColorScheme", "prefers-color-scheme"),
      ("media:prefersReducedMotion", "prefers-reduced-motion"),
      ("media:prefersContrast", "prefers-contrast"),
      ("media:prefersReducedData", "prefers-reduced-data"),
      ("media:anyHover", "any-hover"),
      ("media:anyPointer", "any-pointer"),
      ("media:colorGamut", "color-gamut"),
    ];

    let mut overrides = Vec::new();
    for (config_key, feature_name) in media_mappings {
      if let Some(value) = Self::config_string(config, &[config_key]) {
        let serialized_name = serde_json::to_string(feature_name).ok()?;
        let serialized_value = serde_json::to_string(&value).ok()?;
        overrides.push(format!("[{serialized_name},{serialized_value}]"));
      }
    }

    if overrides.is_empty() {
      return None;
    }

    let overrides_array = overrides.join(",");

    Some(format!(
      r#";(function(){{const om=window.matchMedia.bind(window);const mv=[{overrides_array}];window.matchMedia=function(q){{for(const[f,v]of mv){{if(q.includes('('+f)){{const m=q.match(new RegExp('\\(\\s*'+f.replace(/[.*+?^${{}}()|[\]\\]/g,'\\\\$&')+'\\s*:\\s*([^)]+)\\s*\\)'));const qv=m?m[1].trim():null;const mql=om(q);if(qv!==null){{Object.defineProperty(mql,'matches',{{configurable:true,get:()=>qv===v}});}}Object.defineProperty(mql,'media',{{configurable:true,get:()=>q}});return mql;}}}}return om(q);}};}})()"#
    ))
  }

  async fn apply_runtime_overrides(
    context: &BrowserContext,
    pages: &[Page],
    config: &HashMap<String, serde_json::Value>,
  ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(script) = Self::runtime_override_script(config) else {
      return Ok(());
    };

    context
      .add_init_script(&script)
      .await
      .map_err(|e| format!("Failed to register Camoufox runtime override init script: {e}"))?;

    for page in pages {
      page
        .eval::<serde_json::Value>(&script)
        .await
        .map_err(|e| format!("Failed to apply Camoufox runtime overrides to active page: {e}"))?;
    }

    Ok(())
  }

  fn new() -> Self {
    Self {
      inner: Arc::new(AsyncMutex::new(CamoufoxManagerInner {
        instances: HashMap::new(),
      })),
    }
  }

  pub fn instance() -> &'static CamoufoxManager {
    &CAMOUFOX_LAUNCHER
  }

  #[allow(dead_code)]
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

  pub fn get_profiles_dir(&self) -> PathBuf {
    crate::app_dirs::profiles_dir()
  }

  async fn wait_for_camoufox_process_by_profile(
    &self,
    target_path: &std::path::Path,
  ) -> Option<(u32, String, Option<u16>)> {
    for _ in 0..20 {
      if let Some(found) = self.find_camoufox_process_by_profile(target_path) {
        return Some(found);
      }
      tokio::time::sleep(Duration::from_millis(250)).await;
    }
    None
  }

  async fn get_automation_state(
    &self,
    profile_path: &str,
  ) -> Result<Arc<AsyncMutex<CamoufoxAutomationState>>, Box<dyn std::error::Error + Send + Sync>>
  {
    let target_path = std::path::Path::new(profile_path)
      .canonicalize()
      .unwrap_or_else(|_| std::path::Path::new(profile_path).to_path_buf());

    let inner = self.inner.lock().await;
    for instance in inner.instances.values() {
      if let Some(instance_profile_path) = &instance.profile_path {
        let instance_path = std::path::Path::new(instance_profile_path)
          .canonicalize()
          .unwrap_or_else(|_| std::path::Path::new(instance_profile_path).to_path_buf());
        if instance_path == target_path {
          if let Some(automation) = &instance.automation {
            return Ok(automation.clone());
          }
          return Err(
            "Camoufox automation session is unavailable for this profile. Relaunch the profile from the current app session.".into(),
          );
        }
      }
    }

    Err("No running Camoufox automation session found for the requested profile".into())
  }

  fn tab_id_from_index(index: usize) -> String {
    format!("camoufox-tab-{index}")
  }

  fn parse_tab_index_from_id(tab_id: &str) -> Option<usize> {
    tab_id
      .strip_prefix("camoufox-tab-")
      .and_then(|value| value.parse::<usize>().ok())
  }

  async fn list_pages_from_state(
    state: &mut CamoufoxAutomationState,
  ) -> Result<Vec<Page>, Box<dyn std::error::Error + Send + Sync>> {
    let pages = state
      .active_page
      .context()
      .pages()
      .map_err(|e| format!("Failed to list Camoufox pages: {e}"))?;

    if pages.is_empty() {
      return Err("Camoufox browser context has no open pages".into());
    }

    if state.selected_tab_index >= pages.len() {
      state.selected_tab_index = pages.len().saturating_sub(1);
    }
    state.active_page = pages[state.selected_tab_index].clone();
    Ok(pages)
  }

  pub async fn get_active_page(
    &self,
    profile_path: &str,
  ) -> Result<Page, Box<dyn std::error::Error + Send + Sync>> {
    let state = self.get_automation_state(profile_path).await?;
    let mut guard = state.lock().await;
    let pages = Self::list_pages_from_state(&mut guard).await?;
    let page = pages[guard.selected_tab_index].clone();
    // Ensure automation keeps working when the OS window is minimized/unfocused.
    let _ = Self::prepare_page_for_background_automation(&page).await;
    Ok(page)
  }

  /// Bring page forward, normalize viewport, and force visible/focus semantics.
  /// Safe to call repeatedly; best-effort (never fails the launch path).
  pub async fn prepare_page_for_background_automation(
    page: &Page,
  ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _ = page.bring_to_front().await;
    // Force a real viewport so layout/input coordinates stay valid when minimized.
    let needs_viewport = page
      .viewport_size()
      .ok()
      .flatten()
      .map(|v| v.width <= 0 || v.height <= 0)
      .unwrap_or(true);
    if needs_viewport {
      let _ = page
        .set_viewport_size(Viewport {
          width: 1280,
          height: 900,
        })
        .await;
    }
    // Override visibility/focus APIs that break when the window is not active.
    let _ = page
      .eval::<serde_json::Value>(
        r#"(function(){
          try {
            Object.defineProperty(document, 'hidden', { get: () => false, configurable: true });
            Object.defineProperty(document, 'visibilityState', { get: () => 'visible', configurable: true });
            if (typeof document.hasFocus === 'function') {
              document.hasFocus = () => true;
            }
          } catch (_) {}
          return true;
        })()"#,
      )
      .await;
    Ok(())
  }

  pub async fn list_pages(
    &self,
    profile_path: &str,
  ) -> Result<(Vec<Page>, usize), Box<dyn std::error::Error + Send + Sync>> {
    let state = self.get_automation_state(profile_path).await?;
    let mut guard = state.lock().await;
    let pages = Self::list_pages_from_state(&mut guard).await?;
    Ok((pages, guard.selected_tab_index))
  }

  pub async fn set_selected_tab(
    &self,
    profile_path: &str,
    selector_by: &str,
    selector_value: Option<&str>,
  ) -> Result<(Vec<Page>, usize), Box<dyn std::error::Error + Send + Sync>> {
    let state = self.get_automation_state(profile_path).await?;
    let mut guard = state.lock().await;
    let pages = Self::list_pages_from_state(&mut guard).await?;

    let selected_index = match selector_by {
      "index" => selector_value
        .ok_or_else(|| "Camoufox tab selector by=index requires value".to_string())?
        .parse::<usize>()
        .map_err(|_| "Camoufox tab selector by=index requires an integer value".to_string())?,
      "id" => {
        let value =
          selector_value.ok_or_else(|| "Camoufox tab selector by=id requires value".to_string())?;
        Self::parse_tab_index_from_id(value)
          .ok_or_else(|| format!("Unsupported Camoufox tab id: {value}"))?
      }
      "url" => {
        let value = selector_value
          .ok_or_else(|| "Camoufox tab selector by=url requires value".to_string())?;
        pages
          .iter()
          .position(|page| page.url().map(|url| url.contains(value)).unwrap_or(false))
          .ok_or_else(|| format!("No Camoufox tab matched url {value}"))?
      }
      "title" => {
        let value = selector_value
          .ok_or_else(|| "Camoufox tab selector by=title requires value".to_string())?;
        let mut matched_index = None;
        for (index, page) in pages.iter().enumerate() {
          let title = page
            .title()
            .await
            .map_err(|e| format!("Failed to read Camoufox tab title: {e}"))?;
          if title.contains(value) {
            matched_index = Some(index);
            break;
          }
        }
        matched_index.ok_or_else(|| format!("No Camoufox tab matched title {value}"))?
      }
      other => {
        return Err(format!("Unsupported Camoufox tab selector strategy: {other}").into());
      }
    };

    if selected_index >= pages.len() {
      return Err(
        format!(
          "Camoufox tab index {selected_index} is out of range ({} tabs)",
          pages.len()
        )
        .into(),
      );
    }

    let selected_page = pages[selected_index].clone();
    selected_page
      .bring_to_front()
      .await
      .map_err(|e| format!("Failed to activate Camoufox tab: {e}"))?;
    guard.selected_tab_index = selected_index;
    guard.active_page = selected_page;
    Ok((pages, selected_index))
  }

  pub async fn new_tab(
    &self,
    profile_path: &str,
    url: Option<&str>,
  ) -> Result<(Vec<Page>, usize), Box<dyn std::error::Error + Send + Sync>> {
    let state = self.get_automation_state(profile_path).await?;
    let mut guard = state.lock().await;
    let context = guard.active_page.context();
    let new_page = context
      .new_page()
      .await
      .map_err(|e| format!("Failed to create Camoufox tab: {e}"))?;
    if let Some(target_url) = url {
      new_page
        .goto_builder(target_url)
        .goto()
        .await
        .map_err(|e| format!("Failed to navigate new Camoufox tab: {e}"))?;
    }
    new_page
      .bring_to_front()
      .await
      .map_err(|e| format!("Failed to activate new Camoufox tab: {e}"))?;
    let pages = context
      .pages()
      .map_err(|e| format!("Failed to list Camoufox tabs after creation: {e}"))?;
    let selected_tab_index = pages.len().saturating_sub(1);
    guard.selected_tab_index = selected_tab_index;
    guard.active_page = new_page;
    Ok((pages, selected_tab_index))
  }

  pub async fn close_tab(
    &self,
    profile_path: &str,
    selector_by: Option<&str>,
    selector_value: Option<&str>,
  ) -> Result<(Vec<Page>, Option<usize>), Box<dyn std::error::Error + Send + Sync>> {
    let state = self.get_automation_state(profile_path).await?;
    let mut guard = state.lock().await;
    let context = guard.active_page.context();
    let pages = context
      .pages()
      .map_err(|e| format!("Failed to list Camoufox tabs before close: {e}"))?;

    if pages.is_empty() {
      return Err("Camoufox browser context has no open pages".into());
    }

    let target_index = match selector_by {
      Some("index") => selector_value
        .ok_or_else(|| "Camoufox tab selector by=index requires value".to_string())?
        .parse::<usize>()
        .map_err(|_| "Camoufox tab selector by=index requires an integer value".to_string())?,
      Some("id") => {
        let value =
          selector_value.ok_or_else(|| "Camoufox tab selector by=id requires value".to_string())?;
        Self::parse_tab_index_from_id(value)
          .ok_or_else(|| format!("Unsupported Camoufox tab id: {value}"))?
      }
      Some("url") => {
        let value = selector_value
          .ok_or_else(|| "Camoufox tab selector by=url requires value".to_string())?;
        pages
          .iter()
          .position(|page| page.url().map(|url| url.contains(value)).unwrap_or(false))
          .ok_or_else(|| format!("No Camoufox tab matched url {value}"))?
      }
      Some("title") => {
        let value = selector_value
          .ok_or_else(|| "Camoufox tab selector by=title requires value".to_string())?;
        let mut matched_index = None;
        for (index, page) in pages.iter().enumerate() {
          let title = page
            .title()
            .await
            .map_err(|e| format!("Failed to read Camoufox tab title: {e}"))?;
          if title.contains(value) {
            matched_index = Some(index);
            break;
          }
        }
        matched_index.ok_or_else(|| format!("No Camoufox tab matched title {value}"))?
      }
      Some(other) => {
        return Err(format!("Unsupported Camoufox tab selector strategy: {other}").into());
      }
      None => guard.selected_tab_index,
    };

    if target_index >= pages.len() {
      return Err(
        format!(
          "Camoufox tab index {target_index} is out of range ({} tabs)",
          pages.len()
        )
        .into(),
      );
    }

    let target_page = pages[target_index].clone();
    target_page
      .close(None)
      .await
      .map_err(|e| format!("Failed to close Camoufox tab: {e}"))?;

    let remaining_pages = context
      .pages()
      .map_err(|e| format!("Failed to list Camoufox tabs after close: {e}"))?;

    if remaining_pages.is_empty() {
      guard.selected_tab_index = 0;
      guard.active_page = target_page;
      return Ok((remaining_pages, None));
    }

    let next_index = target_index.min(remaining_pages.len().saturating_sub(1));
    let next_page = remaining_pages[next_index].clone();
    next_page
      .bring_to_front()
      .await
      .map_err(|e| format!("Failed to activate Camoufox tab after close: {e}"))?;
    guard.selected_tab_index = next_index;
    guard.active_page = next_page;
    Ok((remaining_pages, Some(next_index)))
  }

  pub fn camoufox_tab_id(index: usize) -> String {
    Self::tab_id_from_index(index)
  }

  async fn is_automation_running(
    &self,
    automation: &Arc<AsyncMutex<CamoufoxAutomationState>>,
  ) -> bool {
    let mut guard = automation.lock().await;
    Self::list_pages_from_state(&mut guard).await.is_ok()
  }

  /// Generate Camoufox fingerprint configuration during profile creation
  pub async fn generate_fingerprint_config(
    &self,
    _app_handle: &AppHandle,
    profile: &crate::profile::BrowserProfile,
    config: &CamoufoxConfig,
  ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    // Get executable path
    let executable_path = BrowserRunner::instance()
      .get_browser_executable_path(profile)
      .map_err(|e| format!("Failed to get Camoufox executable path: {e}"))?;

    // Build the config using CamoufoxConfigBuilder
    let mut builder = CamoufoxConfigBuilder::new()
      .block_images(config.block_images.unwrap_or(false))
      .block_webrtc(config.block_webrtc.unwrap_or(false))
      .block_webgl(config.block_webgl.unwrap_or(false));

    // Set operating system
    if let Some(os) = &config.os {
      builder = builder.operating_system(os);
    }

    // Build screen constraints if provided
    if config.screen_min_width.is_some()
      || config.screen_max_width.is_some()
      || config.screen_min_height.is_some()
      || config.screen_max_height.is_some()
    {
      let screen_constraints = ScreenConstraints {
        min_width: config.screen_min_width,
        max_width: config.screen_max_width,
        min_height: config.screen_min_height,
        max_height: config.screen_max_height,
      };
      builder = builder.screen_constraints(screen_constraints);
    }

    // Parse proxy if provided
    if let Some(proxy_str) = &config.proxy {
      let proxy_config = crate::camoufox::ProxyConfig::from_url(proxy_str)
        .map_err(|e| format!("Failed to parse proxy URL: {e}"))?;
      builder = builder.proxy(proxy_config);
    }

    // Set Firefox version from executable
    if let Some(version) = crate::camoufox::config::get_firefox_version(&executable_path) {
      builder = builder.ff_version(version);
    }

    // Handle geoip option
    if let Some(geoip_value) = &config.geoip {
      match geoip_value {
        serde_json::Value::Bool(true) => {
          // Auto-detect IP (through proxy if set)
          builder = builder.geoip(GeoIPOption::Auto);
        }
        serde_json::Value::String(ip) => {
          // Use specific IP
          builder = builder.geoip(GeoIPOption::IP(ip.clone()));
        }
        _ => {
          // geoip: false or other values - don't apply geolocation
        }
      }
    }

    // Build the config (async to handle geoip)
    let launch_config = builder
      .build_async()
      .await
      .map_err(|e| format!("Failed to build Camoufox config: {e}"))?;

    // Return the fingerprint config as JSON
    let config_json = serde_json::to_string(&launch_config.fingerprint_config)
      .map_err(|e| format!("Failed to serialize config: {e}"))?;

    Ok(config_json)
  }

  /// Launch Camoufox browser by directly spawning the process
  pub async fn launch_camoufox(
    &self,
    _app_handle: &AppHandle,
    profile: &crate::profile::BrowserProfile,
    profile_path: &str,
    config: &CamoufoxConfig,
    url: Option<&str>,
    headless: bool,
  ) -> Result<CamoufoxLaunchResult, Box<dyn std::error::Error + Send + Sync>> {
    let custom_config = if let Some(existing_fingerprint) = &config.fingerprint {
      log::info!("Using existing fingerprint from profile metadata");
      existing_fingerprint.clone()
    } else {
      return Err("No fingerprint provided".into());
    };

    // Get executable path
    let executable_path = BrowserRunner::instance()
      .get_browser_executable_path(profile)
      .map_err(|e| format!("Failed to get Camoufox executable path: {e}"))?;

    let fingerprint_config: HashMap<String, serde_json::Value> =
      serde_json::from_str(&custom_config)
        .map_err(|e| format!("Failed to parse fingerprint config: {e}"))?;

    let screen = if config.screen_min_width.is_some()
      || config.screen_max_width.is_some()
      || config.screen_min_height.is_some()
      || config.screen_max_height.is_some()
    {
      Some(ScreenConstraints {
        min_width: config.screen_min_width,
        max_width: config.screen_max_width,
        min_height: config.screen_min_height,
        max_height: config.screen_max_height,
      })
    } else {
      None
    };

    let proxy = config
      .proxy
      .as_deref()
      .map(crate::camoufox::ProxyConfig::from_url)
      .transpose()
      .map_err(|e| format!("Failed to parse proxy URL: {e}"))?;

    let canonical_profile_path = std::path::Path::new(profile_path)
      .canonicalize()
      .unwrap_or_else(|_| std::path::Path::new(profile_path).to_path_buf());

    let launcher = Arc::new(
      CamoufoxLauncher::new(&executable_path)
        .await
        .map_err(|e| format!("Failed to initialize Camoufox launcher: {e}"))?,
    );

    let effective_headless = headless || std::env::var("CAMOUFOX_HEADLESS").is_ok();
    let context = launcher
      .launch_persistent_context(
        &canonical_profile_path,
        LaunchOptions {
          os: config.os.clone(),
          block_images: config.block_images.unwrap_or(false),
          block_webrtc: config.block_webrtc.unwrap_or(false),
          block_webgl: config.block_webgl.unwrap_or(false),
          screen,
          fingerprint: None,
          extra_config: Some(fingerprint_config.clone()),
          headless: effective_headless,
          proxy,
          debug: false,
          ..Default::default()
        },
      )
      .await
      .map_err(|e| format!("Failed to launch Camoufox persistent context: {e}"))?;

    let _ = context.set_default_timeout(30_000).await;
    let _ = context.set_default_navigation_timeout(30_000).await;

    let initial_pages = context
      .pages()
      .map_err(|e| format!("Failed to list Camoufox pages after launch: {e}"))?;
    Self::apply_runtime_overrides(&context, &initial_pages, &fingerprint_config).await?;

    let mut pages = context
      .pages()
      .map_err(|e| format!("Failed to list Camoufox pages after launch: {e}"))?;
    let selected_tab_index = if pages.is_empty() {
      0
    } else {
      pages.len().saturating_sub(1)
    };

    let page = if pages.is_empty() {
      let page = context
        .new_page()
        .await
        .map_err(|e| format!("Failed to create initial Camoufox page: {e}"))?;
      pages.push(page.clone());
      page
    } else {
      pages[selected_tab_index].clone()
    };

    if let Some(target_url) = url {
      page
        .goto_builder(target_url)
        .goto()
        .await
        .map_err(|e| format!("Failed to navigate initial Camoufox page: {e}"))?;
    }

    page
      .bring_to_front()
      .await
      .map_err(|e| format!("Failed to activate initial Camoufox page: {e}"))?;
    let _ = Self::prepare_page_for_background_automation(&page).await;

    let process_id = self
      .wait_for_camoufox_process_by_profile(&canonical_profile_path)
      .await
      .map(|(pid, _, _)| pid);
    let instance_id = format!(
      "camoufox_{}",
      process_id.map_or_else(
        || {
          SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis().to_string())
            .unwrap_or_else(|_| "0".to_string())
        },
        |pid| pid.to_string()
      )
    );

    log::info!("Camoufox launched with PID: {:?}", process_id);

    // Store the instance
    let instance = CamoufoxInstance {
      id: instance_id.clone(),
      process_id,
      profile_path: Some(profile_path.to_string()),
      url: url.map(String::from),
      cdp_port: None,
      automation: Some(Arc::new(AsyncMutex::new(CamoufoxAutomationState {
        active_page: page.clone(),
        selected_tab_index,
      }))),
      launcher: Some(launcher),
    };

    let launch_result = CamoufoxLaunchResult {
      id: instance_id.clone(),
      processId: process_id,
      profilePath: Some(profile_path.to_string()),
      url: url.map(String::from),
      cdp_port: None,
    };

    {
      let mut inner = self.inner.lock().await;
      inner.instances.insert(instance_id, instance);
    }

    Ok(launch_result)
  }

  /// Stop a Camoufox process by ID
  pub async fn stop_camoufox(
    &self,
    _app_handle: &AppHandle,
    id: &str,
  ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    // Get the process ID from our tracking
    let (process_id, automation) = {
      let inner = self.inner.lock().await;
      inner.instances.get(id).map_or((None, None), |instance| {
        (instance.process_id, instance.automation.clone())
      })
    };

    if let Some(ref automation) = automation {
      let active_page = automation.lock().await.active_page.clone();
      if let Err(error) = active_page.context().close().await {
        log::warn!("Failed to close Camoufox browser context for {id}: {error}");
      }
    }

    if let Some(pid) = process_id {
      // Kill the process
      let success = self.kill_process(pid);

      if success {
        // Remove from our tracking
        let mut inner = self.inner.lock().await;
        inner.instances.remove(id);
        log::info!("Stopped Camoufox instance {} (PID: {})", id, pid);
      }

      Ok(success)
    } else {
      // No process ID found, fall back to automation/context closure tracking
      let mut inner = self.inner.lock().await;
      inner.instances.remove(id);
      Ok(automation.is_some())
    }
  }

  /// Kill a process by PID
  fn kill_process(&self, pid: u32) -> bool {
    #[cfg(unix)]
    {
      use std::os::unix::process::ExitStatusExt;
      let result = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status();

      match result {
        Ok(status) => status.success() || status.signal() == Some(0),
        Err(e) => {
          log::warn!("Failed to kill process {}: {}", pid, e);
          false
        }
      }
    }

    #[cfg(windows)]
    {
      use std::os::windows::process::CommandExt;
      const CREATE_NO_WINDOW: u32 = 0x08000000;
      // /F /T: force-kill full process tree (matches chromium + platform_browser).
      let result = std::process::Command::new("taskkill")
        .args(["/F", "/T", "/PID", &pid.to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .status();

      match result {
        Ok(status) => {
          if status.success() {
            true
          } else {
            // Already-gone PID is success for concurrent worker teardown.
            use sysinfo::{Pid, System};
            let system = System::new_all();
            system.process(Pid::from_u32(pid)).is_none()
          }
        }
        Err(e) => {
          log::warn!("Failed to kill process {}: {}", pid, e);
          false
        }
      }
    }
  }

  /// Find Camoufox server by profile path (for integration with browser_runner)
  /// This method first checks in-memory instances, then scans system processes
  /// to detect Camoufox instances that may have been started before the app restarted.
  pub async fn find_camoufox_by_profile(
    &self,
    profile_path: &str,
  ) -> Result<Option<CamoufoxLaunchResult>, Box<dyn std::error::Error + Send + Sync>> {
    // First clean up any dead instances
    self.cleanup_dead_instances().await?;

    // Convert paths to canonical form for comparison
    let target_path = std::path::Path::new(profile_path)
      .canonicalize()
      .unwrap_or_else(|_| std::path::Path::new(profile_path).to_path_buf());

    // Check in-memory instances first
    {
      let inner = self.inner.lock().await;

      for (id, instance) in inner.instances.iter() {
        if let Some(instance_profile_path) = &instance.profile_path {
          let instance_path = std::path::Path::new(instance_profile_path)
            .canonicalize()
            .unwrap_or_else(|_| std::path::Path::new(instance_profile_path).to_path_buf());

          if instance_path == target_path {
            let automation_running = if let Some(automation) = &instance.automation {
              self.is_automation_running(automation).await
            } else {
              false
            };

            let process_running = if let Some(process_id) = instance.process_id {
              self.is_server_running(process_id).await
            } else {
              false
            };

            if automation_running || process_running {
              return Ok(Some(CamoufoxLaunchResult {
                id: id.clone(),
                processId: instance.process_id,
                profilePath: instance.profile_path.clone(),
                url: instance.url.clone(),
                cdp_port: instance.cdp_port,
              }));
            }
          }
        }
      }
    }

    // If not found in in-memory instances, scan system processes
    // This handles the case where the app was restarted but Camoufox is still running
    if let Some((pid, found_profile_path, cdp_port)) =
      self.find_camoufox_process_by_profile(&target_path)
    {
      log::info!(
        "Found running Camoufox process (PID: {}) for profile path via system scan",
        pid
      );

      // Register this instance in our tracking
      let instance_id = format!("recovered_{}", pid);
      let mut inner = self.inner.lock().await;
      inner.instances.insert(
        instance_id.clone(),
        CamoufoxInstance {
          id: instance_id.clone(),
          process_id: Some(pid),
          profile_path: Some(found_profile_path.clone()),
          url: None,
          cdp_port,
          automation: None,
          launcher: None,
        },
      );

      return Ok(Some(CamoufoxLaunchResult {
        id: instance_id,
        processId: Some(pid),
        profilePath: Some(found_profile_path),
        url: None,
        cdp_port,
      }));
    }

    Ok(None)
  }

  /// Scan system processes to find a Camoufox process using a specific profile path
  fn find_camoufox_process_by_profile(
    &self,
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

      // Check if this is a Camoufox/Firefox process
      let exe_name = process.name().to_string_lossy().to_lowercase();
      let is_firefox_like = exe_name.contains("firefox")
        || exe_name.contains("camoufox")
        || exe_name.contains("firefox-bin");

      if !is_firefox_like {
        continue;
      }

      let mut matched = false;
      let mut found_profile_path = None;
      let mut cdp_port: Option<u16> = None;

      // Check if the command line contains our profile path
      for (i, arg) in cmd.iter().enumerate() {
        if let Some(arg_str) = arg.to_str() {
          // Check for -profile argument followed by our path
          if arg_str == "-profile" && i + 1 < cmd.len() {
            if let Some(next_arg) = cmd.get(i + 1).and_then(|a| a.to_str()) {
              let cmd_path = std::path::Path::new(next_arg)
                .canonicalize()
                .unwrap_or_else(|_| std::path::Path::new(next_arg).to_path_buf());

              if cmd_path == target_path {
                matched = true;
                found_profile_path = Some(next_arg.to_string());
              }
            }
          }

          // Also check if the argument contains the profile path directly
          if !matched && arg_str.contains(&*target_path_str) {
            matched = true;
            found_profile_path = Some(target_path_str.to_string());
          }

          if let Some(port_val) = arg_str.strip_prefix("--remote-debugging-port=") {
            cdp_port = port_val.parse().ok();
          }
        }
      }

      if matched {
        if let Some(profile_path) = found_profile_path {
          return Some((pid.as_u32(), profile_path, cdp_port));
        }
      }
    }

    None
  }

  /// Check if servers are still alive and clean up dead instances
  pub async fn cleanup_dead_instances(
    &self,
  ) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
    let mut dead_instances = Vec::new();
    let mut instances_to_remove = Vec::new();

    {
      let inner = self.inner.lock().await;

      for (id, instance) in inner.instances.iter() {
        let automation_running = if let Some(automation) = &instance.automation {
          self.is_automation_running(automation).await
        } else {
          false
        };

        let process_running = if let Some(process_id) = instance.process_id {
          self.is_server_running(process_id).await
        } else {
          false
        };

        if !automation_running && !process_running {
          dead_instances.push(id.clone());
          instances_to_remove.push(id.clone());
        }
      }
    }

    // Remove dead instances
    if !instances_to_remove.is_empty() {
      let mut inner = self.inner.lock().await;
      for id in &instances_to_remove {
        inner.instances.remove(id);
        // Removed dead Camoufox instance
      }
    }

    Ok(dead_instances)
  }

  /// Check if a Camoufox server is running with the given process ID
  async fn is_server_running(&self, process_id: u32) -> bool {
    // Check if the process is still running
    use sysinfo::{Pid, ProcessRefreshKind, RefreshKind, System};

    let system = System::new_with_specifics(
      RefreshKind::nothing().with_processes(ProcessRefreshKind::everything()),
    );
    if let Some(process) = system.process(Pid::from(process_id as usize)) {
      // Check if this is actually a Camoufox process by looking at the command line
      let cmd = process.cmd();
      let is_camoufox = cmd.iter().any(|arg| {
        let arg_str = arg.to_str().unwrap_or("");
        arg_str.contains("camoufox-worker") || arg_str.contains("camoufox")
      });

      if is_camoufox {
        // Found running Camoufox process
        return true;
      }
    }

    false
  }
}

impl CamoufoxManager {
  pub async fn launch_camoufox_profile(
    &self,
    app_handle: AppHandle,
    profile: BrowserProfile,
    config: CamoufoxConfig,
    url: Option<String>,
    override_profile_path: Option<std::path::PathBuf>,
    headless: bool,
  ) -> Result<CamoufoxLaunchResult, String> {
    // Get profile path
    let profile_path = if let Some(ref override_path) = override_profile_path {
      override_path.clone()
    } else {
      let profiles_dir = self.get_profiles_dir();
      profile.get_profile_data_path(&profiles_dir)
    };
    let profile_path_str = profile_path.to_string_lossy();

    // Check if there's already a running instance for this profile
    if let Ok(Some(existing)) = self.find_camoufox_by_profile(&profile_path_str).await {
      // If there's an existing instance, stop it first to avoid conflicts
      let _ = self.stop_camoufox(&app_handle, &existing.id).await;
    }

    // Clean up any dead instances before launching
    let _ = self.cleanup_dead_instances().await;

    // Always ensure background-automation prefs exist (persistent contexts cannot
    // take firefox_user_prefs via Playwright).
    {
      let user_js_path = profile_path.join("user.js");
      let mut prefs = String::new();
      if let Ok(existing) = std::fs::read_to_string(&user_js_path) {
        prefs = existing;
      }
      if override_profile_path.is_some() && !prefs.contains("browser.cache.disk.enable") {
        prefs.push_str(concat!(
          "user_pref(\"browser.cache.disk.enable\", false);\n",
          "user_pref(\"browser.cache.memory.enable\", true);\n",
          "user_pref(\"browser.sessionstore.resume_from_crash\", false);\n",
          "user_pref(\"browser.sessionstore.max_tabs_undo\", 0);\n",
          "user_pref(\"browser.sessionstore.max_windows_undo\", 0);\n",
          "user_pref(\"places.history.enabled\", false);\n",
          "user_pref(\"browser.formfill.enable\", false);\n",
          "user_pref(\"signon.rememberSignons\", false);\n",
          "user_pref(\"browser.bookmarks.max_backups\", 0);\n",
          "user_pref(\"browser.shell.checkDefaultBrowser\", false);\n",
          "user_pref(\"toolkit.crashreporter.enabled\", false);\n",
          "user_pref(\"browser.pagethumbnails.capturing_disabled\", true);\n",
          "user_pref(\"browser.download.manager.addToRecentDocs\", false);\n",
        ));
      }
      if !prefs.contains("dom.min_background_timeout_value") {
        if !prefs.ends_with('\n') && !prefs.is_empty() {
          prefs.push('\n');
        }
        prefs.push_str(crate::camoufox::launcher::automation_background_user_js_lines());
      }
      if let Err(e) = std::fs::write(&user_js_path, prefs) {
        log::warn!("Failed to write Camoufox user.js automation prefs: {e}");
      }
    }

    // Write explicit proxy prefs to user.js so Firefox always uses the local
    // donut-proxy and never falls back to stale proxy settings baked into prefs.js
    // from a previous session. user.js values override prefs.js on every launch.
    if let Some(proxy_str) = &config.proxy {
      let user_js_path = profile_path.join("user.js");
      let mut prefs = String::new();

      // Preserve existing user.js content (ephemeral prefs, etc.)
      if let Ok(existing) = std::fs::read_to_string(&user_js_path) {
        // Strip old proxy prefs so we don't duplicate
        for line in existing.lines() {
          if !line.contains("network.proxy.") {
            prefs.push_str(line);
            prefs.push('\n');
          }
        }
      }

      if let Ok(parsed) = url::Url::parse(proxy_str) {
        let host = parsed.host_str().unwrap_or("127.0.0.1");
        let port = parsed.port().unwrap_or(8080);
        let scheme = parsed.scheme();

        if scheme == "socks5" || scheme == "socks4" {
          prefs.push_str(&format!(
            "user_pref(\"network.proxy.type\", 1);\n\
             user_pref(\"network.proxy.socks\", \"{host}\");\n\
             user_pref(\"network.proxy.socks_port\", {port});\n\
             user_pref(\"network.proxy.socks_version\", {});\n\
             user_pref(\"network.proxy.socks_remote_dns\", true);\n",
            if scheme == "socks5" { 5 } else { 4 }
          ));
        } else {
          // HTTP/HTTPS proxy
          prefs.push_str(&format!(
            "user_pref(\"network.proxy.type\", 1);\n\
             user_pref(\"network.proxy.http\", \"{host}\");\n\
             user_pref(\"network.proxy.http_port\", {port});\n\
             user_pref(\"network.proxy.ssl\", \"{host}\");\n\
             user_pref(\"network.proxy.ssl_port\", {port});\n\
             user_pref(\"network.proxy.no_proxies_on\", \"\");\n"
          ));
        }

        if let Err(e) = std::fs::write(&user_js_path, prefs) {
          log::warn!("Failed to write proxy prefs to user.js: {e}");
        }
      }
    }

    self
      .launch_camoufox(
        &app_handle,
        &profile,
        &profile_path_str,
        &config,
        url.as_deref(),
        headless,
      )
      .await
      .map_err(|e| format!("Failed to launch Camoufox: {e}"))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_default_config() {
    let default_config = CamoufoxConfig::default();

    // Verify defaults
    assert_eq!(default_config.geoip, Some(serde_json::Value::Bool(true)));
    assert_eq!(default_config.proxy, None);
    assert_eq!(default_config.fingerprint, None);
    assert_eq!(default_config.randomize_fingerprint_on_launch, None);
    assert_eq!(default_config.os, None);
  }

  #[test]
  fn camoufox_automation_background_prefs_disable_timer_throttling() {
    let prefs = crate::camoufox::launcher::automation_background_firefox_prefs();
    assert_eq!(
      prefs.get("dom.min_background_timeout_value"),
      Some(&serde_json::json!(0))
    );
    assert_eq!(
      prefs.get("dom.timeout.enable_budget_timer_throttling"),
      Some(&serde_json::json!(false))
    );
    let lines = crate::camoufox::launcher::automation_background_user_js_lines();
    assert!(lines.contains("dom.min_background_timeout_value"));
    assert!(lines.contains("dom.timeout.enable_budget_timer_throttling"));
  }

  #[test]
  fn test_config_languages_falls_back_to_locale() {
    let config = HashMap::from([
      ("locale:language".to_string(), serde_json::json!("zh")),
      ("locale:region".to_string(), serde_json::json!("CN")),
    ]);

    let languages =
      CamoufoxManager::config_languages(&config).expect("languages should derive from locale");
    assert_eq!(languages, vec!["zh-CN".to_string(), "zh".to_string()]);
  }

  #[test]
  fn test_runtime_override_script_contains_expected_fingerprint_fields() {
    let config = HashMap::from([
      (
        "navigator.userAgent".to_string(),
        serde_json::json!(
          "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:146.0) Gecko/20100101 Firefox/146.0"
        ),
      ),
      ("navigator.platform".to_string(), serde_json::json!("Win32")),
      (
        "navigator.hardwareConcurrency".to_string(),
        serde_json::json!(4),
      ),
      ("locale:language".to_string(), serde_json::json!("zh")),
      ("locale:region".to_string(), serde_json::json!("CN")),
      ("timezone".to_string(), serde_json::json!("Asia/Shanghai")),
    ]);

    let script = CamoufoxManager::runtime_override_script(&config)
      .expect("runtime override script should be generated");

    assert!(script.contains("Firefox/146.0"));
    assert!(script.contains("Win32"));
    assert!(script.contains("\"zh-CN\""));
    assert!(script.contains("hardwareConcurrency"));
    assert!(script.contains("Asia/Shanghai"));
    assert!(script.contains("resolvedOptions"));
  }

  #[test]
  fn test_runtime_override_script_with_match_media_is_valid_js() {
    // Media overrides used to concatenate as `})()return true` which breaks ASI and
    // makes Playwright page.eval fail with: unexpected token: keyword 'return'.
    let config = HashMap::from([
      (
        "navigator.userAgent".to_string(),
        serde_json::json!(
          "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:135.0) Gecko/20100101 Firefox/135.0"
        ),
      ),
      ("navigator.platform".to_string(), serde_json::json!("Win32")),
      (
        "timezone".to_string(),
        serde_json::json!("Asia/Ho_Chi_Minh"),
      ),
      (
        "media:prefersColorScheme".to_string(),
        serde_json::json!("light"),
      ),
      (
        "media:prefersReducedMotion".to_string(),
        serde_json::json!("no-preference"),
      ),
    ]);

    let script = CamoufoxManager::runtime_override_script(&config)
      .expect("runtime override script should be generated with matchMedia");

    assert!(
      script.contains("})();return true"),
      "matchMedia IIFE must be semicolon-terminated before return true; got tail: {}",
      &script[script.len().saturating_sub(40)..]
    );
    assert!(script.contains("prefers-color-scheme"));
    assert!(
      !script.contains("})()return true"),
      "ASI-breaking concatenation must not appear"
    );
  }
}

// Global singleton instance
lazy_static::lazy_static! {
  static ref CAMOUFOX_LAUNCHER: CamoufoxManager = CamoufoxManager::new();
}

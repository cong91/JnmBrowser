use axum::{
  body::Body,
  extract::State,
  http::{header, Request, StatusCode},
  middleware::{self, Next},
  response::{IntoResponse, Response},
  routing::{get, post},
  Json, Router,
};
use base64::Engine;
use futures_util::{stream, StreamExt};
use playwright::api::{Page, Request as PlaywrightRequest};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use tauri::AppHandle;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use uuid::Uuid;

use crate::browser::ProxySettings;
use crate::cloud_auth::CLOUD_AUTH;
use crate::group_manager::GROUP_MANAGER;
use crate::profile::{BrowserProfile, ProfileManager};
use crate::proxy_manager::PROXY_MANAGER;
use crate::settings_manager::SettingsManager;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpTool {
  pub name: String,
  pub description: String,
  pub input_schema: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct McpRequest {
  jsonrpc: String,
  id: Option<serde_json::Value>,
  method: String,
  params: Option<serde_json::Value>,
}

const PROTOCOL_VERSION: &str = "2025-11-25";
const SERVER_NAME: &str = "JnmBrowser";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Serialize)]
pub struct McpResponse {
  jsonrpc: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  id: Option<serde_json::Value>,
  #[serde(skip_serializing_if = "Option::is_none")]
  result: Option<serde_json::Value>,
  #[serde(skip_serializing_if = "Option::is_none")]
  error: Option<McpError>,
}

#[derive(Debug, Serialize)]
pub struct McpError {
  code: i32,
  message: String,
}

const MCP_ERROR_INVALID_PARAMS: i32 = -32602;
const MCP_ERROR_METHOD_NOT_FOUND: i32 = -32601;
const MCP_ERROR_NOT_RUNNING: i32 = -32001;
const MCP_ERROR_INTERNAL: i32 = -32000;
const MCP_ERROR_NOT_FOUND: i32 = -32004;
const MCP_ERROR_TIMEOUT: i32 = -32008;
const DEFAULT_MCP_PORT: u16 = 51080;
const DEFAULT_CONSOLE_CAPTURE_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_CONSOLE_LOG_LIMIT: usize = 100;
const MAX_CONSOLE_LOG_ENTRIES: usize = 500;
const DEFAULT_NETWORK_CAPTURE_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_NETWORK_REQUEST_LIMIT: usize = 100;
const MAX_NETWORK_REQUEST_ENTRIES: usize = 500;
const DEFAULT_RESPONSE_BODY_MAX_BYTES: usize = 256 * 1024;
const DEFAULT_WAIT_TIMEOUT_MS: u64 = 10_000;
const WAIT_POLL_INTERVAL_MS: u64 = 200;

fn is_supported_mcp_browser(browser: &str) -> bool {
  let normalized = crate::browser::normalize_browser_name(browser);
  normalized == "chromium" || normalized == "camoufox"
}

fn is_mcp_chromium_browser(browser: &str) -> bool {
  crate::browser::is_chromium_browser_name(browser)
}

fn canonical_mcp_browser_name(browser: &str) -> &str {
  if is_mcp_chromium_browser(browser) {
    "chromium"
  } else {
    browser
  }
}

fn serialize_profile_for_mcp(profile: &BrowserProfile) -> serde_json::Value {
  let mut value = serde_json::to_value(profile).unwrap_or(serde_json::Value::Null);
  if let Some(obj) = value.as_object_mut() {
    obj.insert(
      "browser".to_string(),
      serde_json::Value::String(canonical_mcp_browser_name(&profile.browser).to_string()),
    );
  }
  value
}

fn mcp_profile_path(profile: &BrowserProfile) -> String {
  crate::ephemeral_dirs::get_effective_profile_path(
    profile,
    &ProfileManager::instance().get_profiles_dir(),
  )
  .to_string_lossy()
  .to_string()
}

impl McpError {
  fn invalid_params(message: impl Into<String>) -> Self {
    Self {
      code: MCP_ERROR_INVALID_PARAMS,
      message: message.into(),
    }
  }

  fn method_not_found(message: impl Into<String>) -> Self {
    Self {
      code: MCP_ERROR_METHOD_NOT_FOUND,
      message: message.into(),
    }
  }

  fn not_running() -> Self {
    Self {
      code: MCP_ERROR_NOT_RUNNING,
      message: "MCP server is not running".to_string(),
    }
  }

  fn internal(message: impl Into<String>) -> Self {
    Self {
      code: MCP_ERROR_INTERNAL,
      message: message.into(),
    }
  }

  fn not_found(message: impl Into<String>) -> Self {
    Self {
      code: MCP_ERROR_NOT_FOUND,
      message: message.into(),
    }
  }

  fn timeout(message: impl Into<String>) -> Self {
    Self {
      code: MCP_ERROR_TIMEOUT,
      message: message.into(),
    }
  }
}

impl McpResponse {
  fn success(id: serde_json::Value, result: serde_json::Value) -> Self {
    Self {
      jsonrpc: "2.0".to_string(),
      id: Some(id),
      result: Some(result),
      error: None,
    }
  }

  fn failure(id: serde_json::Value, error: McpError) -> Self {
    Self {
      jsonrpc: "2.0".to_string(),
      id: Some(id),
      result: None,
      error: Some(error),
    }
  }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct McpCommonOptions {
  #[serde(alias = "timeoutMs")]
  timeout_ms: Option<u64>,
  #[serde(alias = "autoWait")]
  auto_wait: Option<bool>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum McpLocatorBy {
  Css,
  Text,
  Testid,
  Xpath,
  Role,
  Label,
  Placeholder,
}

impl McpLocatorBy {
  fn schema_values() -> &'static [&'static str] {
    &[
      "css",
      "text",
      "testid",
      "xpath",
      "role",
      "label",
      "placeholder",
    ]
  }

  fn as_str(&self) -> &'static str {
    match self {
      Self::Css => "css",
      Self::Text => "text",
      Self::Testid => "testid",
      Self::Xpath => "xpath",
      Self::Role => "role",
      Self::Label => "label",
      Self::Placeholder => "placeholder",
    }
  }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct McpLocator {
  by: McpLocatorBy,
  value: String,
  nth: Option<usize>,
  exact: Option<bool>,
  name: Option<String>,
}

impl McpLocator {
  fn validate(&self) -> Result<(), McpError> {
    if self.value.trim().is_empty() {
      return Err(McpError::invalid_params(
        "Locator value cannot be empty or whitespace only",
      ));
    }

    match (&self.by, &self.name) {
      (McpLocatorBy::Role, Some(name)) if name.trim().is_empty() => {
        return Err(McpError::invalid_params(
          "Role locator name cannot be empty or whitespace only",
        ));
      }
      (McpLocatorBy::Role, _) => {}
      (_, Some(_)) => {
        return Err(McpError::invalid_params(
          "Locator name is only supported for role locators",
        ));
      }
      (_, None) => {}
    }

    Ok(())
  }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct LocatorResolutionSnapshot {
  locator: McpLocator,
  strategy: McpLocatorBy,
  index: usize,
  count: usize,
  exists: bool,
  visible: bool,
  text: String,
  matched_texts: Vec<String>,
  #[serde(default)]
  ambiguous: bool,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  ambiguity_message: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum McpFrameBy {
  Main,
  Index,
  Url,
  Name,
  Id,
}

#[allow(dead_code)]
impl McpFrameBy {
  fn schema_values() -> &'static [&'static str] {
    &["main", "index", "url", "name", "id"]
  }

  fn as_str(&self) -> &'static str {
    match self {
      Self::Main => "main",
      Self::Index => "index",
      Self::Url => "url",
      Self::Name => "name",
      Self::Id => "id",
    }
  }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
enum McpFrameValue {
  Text(String),
  Index(usize),
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct McpFrameRef {
  by: McpFrameBy,
  value: Option<McpFrameValue>,
}

#[allow(dead_code)]
impl McpFrameRef {
  fn validate(&self) -> Result<(), McpError> {
    match (&self.by, &self.value) {
      (McpFrameBy::Main, None) => Ok(()),
      (McpFrameBy::Main, Some(_)) => Err(McpError::invalid_params(
        "Frame selector by=main must not include value",
      )),
      (McpFrameBy::Index, Some(McpFrameValue::Index(_))) => Ok(()),
      (McpFrameBy::Index, Some(McpFrameValue::Text(_))) => Err(McpError::invalid_params(
        "Frame selector by=index requires an integer value",
      )),
      (McpFrameBy::Index, None) => Err(McpError::invalid_params(
        "Frame selector by=index requires value",
      )),
      (McpFrameBy::Url | McpFrameBy::Name | McpFrameBy::Id, Some(McpFrameValue::Text(value)))
        if !value.trim().is_empty() =>
      {
        Ok(())
      }
      (McpFrameBy::Url | McpFrameBy::Name | McpFrameBy::Id, Some(McpFrameValue::Text(_))) => Err(
        McpError::invalid_params("Frame selector text value cannot be empty or whitespace only"),
      ),
      (McpFrameBy::Url | McpFrameBy::Name | McpFrameBy::Id, Some(McpFrameValue::Index(_))) => Err(
        McpError::invalid_params("Frame selector by=url/name/id requires a string value"),
      ),
      (McpFrameBy::Url | McpFrameBy::Name | McpFrameBy::Id, None) => Err(McpError::invalid_params(
        "Frame selector by=url/name/id requires value",
      )),
    }
  }

  fn describe(&self) -> String {
    match (&self.by, &self.value) {
      (McpFrameBy::Main, _) => "frame main".to_string(),
      (McpFrameBy::Index, Some(McpFrameValue::Index(index))) => format!("frame index {index}"),
      (McpFrameBy::Url, Some(McpFrameValue::Text(value))) => format!("frame url {value}"),
      (McpFrameBy::Name, Some(McpFrameValue::Text(value))) => format!("frame name {value}"),
      (McpFrameBy::Id, Some(McpFrameValue::Text(value))) => format!("frame id {value}"),
      _ => format!("frame {}", self.by.as_str()),
    }
  }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct McpFrameNode {
  id: String,
  parent_id: Option<String>,
  index: usize,
  depth: usize,
  url: String,
  name: Option<String>,
  origin: Option<String>,
  main: bool,
  child_count: usize,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct McpFrameTreeNode {
  id: String,
  parent_id: Option<String>,
  index: usize,
  depth: usize,
  url: String,
  name: Option<String>,
  origin: Option<String>,
  main: bool,
  children: Vec<McpFrameTreeNode>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum McpTabBy {
  Id,
  Index,
  Url,
  Title,
}

#[allow(dead_code)]
impl McpTabBy {
  fn schema_values() -> &'static [&'static str] {
    &["id", "index", "url", "title"]
  }

  fn as_str(&self) -> &'static str {
    match self {
      Self::Id => "id",
      Self::Index => "index",
      Self::Url => "url",
      Self::Title => "title",
    }
  }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
enum McpTabValue {
  Text(String),
  Index(usize),
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct McpTabRef {
  by: McpTabBy,
  value: Option<McpTabValue>,
}

#[allow(dead_code)]
impl McpTabRef {
  fn validate(&self) -> Result<(), McpError> {
    match (&self.by, &self.value) {
      (McpTabBy::Index, Some(McpTabValue::Index(_))) => Ok(()),
      (McpTabBy::Index, Some(McpTabValue::Text(value))) => value
        .trim()
        .parse::<usize>()
        .map(|_| ())
        .map_err(|_| McpError::invalid_params("Tab selector by=index requires an integer value")),
      (McpTabBy::Index, None) => Err(McpError::invalid_params(
        "Tab selector by=index requires value",
      )),
      (McpTabBy::Id | McpTabBy::Url | McpTabBy::Title, Some(McpTabValue::Text(value)))
        if !value.trim().is_empty() =>
      {
        Ok(())
      }
      (McpTabBy::Id | McpTabBy::Url | McpTabBy::Title, Some(McpTabValue::Text(_))) => Err(
        McpError::invalid_params("Tab selector text value cannot be empty or whitespace only"),
      ),
      (McpTabBy::Id | McpTabBy::Url | McpTabBy::Title, Some(McpTabValue::Index(_))) => Err(
        McpError::invalid_params("Tab selector by=id/url/title requires a string value"),
      ),
      (McpTabBy::Id | McpTabBy::Url | McpTabBy::Title, None) => Err(McpError::invalid_params(
        "Tab selector by=id/url/title requires value",
      )),
    }
  }

  fn describe(&self) -> String {
    match (&self.by, &self.value) {
      (McpTabBy::Id, Some(McpTabValue::Text(value))) => format!("tab id {value}"),
      (McpTabBy::Index, Some(McpTabValue::Index(index))) => format!("tab index {index}"),
      (McpTabBy::Index, Some(McpTabValue::Text(value))) => format!("tab index {value}"),
      (McpTabBy::Url, Some(McpTabValue::Text(value))) => format!("tab url {value}"),
      (McpTabBy::Title, Some(McpTabValue::Text(value))) => format!("tab title {value}"),
      _ => format!("tab {}", self.by.as_str()),
    }
  }
}

#[derive(Debug, Deserialize)]
struct StartConsoleCaptureArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  #[serde(flatten)]
  options: McpCommonOptions,
}

#[derive(Debug, Deserialize)]
struct GetConsoleLogsArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ClearConsoleLogsArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
}

#[derive(Debug, Deserialize)]
struct StartNetworkCaptureArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  #[serde(flatten)]
  options: McpCommonOptions,
}

#[derive(Debug, Deserialize)]
struct GetNetworkRequestsArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ClearNetworkRequestsArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
}

#[derive(Debug, Deserialize)]
struct GetResponseBodyArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  #[serde(alias = "requestId")]
  request_id: String,
  #[serde(alias = "maxBytes")]
  max_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct GetTrafficStatsForPeriodArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  seconds: u64,
}

#[derive(Debug, Deserialize)]
struct SaveSyncSettingsArgs {
  #[serde(alias = "syncServerUrl")]
  sync_server_url: Option<String>,
  #[serde(alias = "syncToken")]
  sync_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RequestProfileSyncArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ValidateRecipeArgs {
  recipe: McpRecipe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PreviewRecipeArgs {
  recipe: McpRecipe,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunRecipeArgs {
  recipe: McpRecipe,
  inputs: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunBatchProfileWorkflowArgs {
  #[serde(alias = "profile_ids")]
  profile_ids: Vec<String>,
  recipe: McpRecipe,
  inputs: Option<serde_json::Value>,
  #[serde(alias = "max_concurrency")]
  max_concurrency: Option<usize>,
  #[serde(alias = "stop_on_profile_failure")]
  stop_on_profile_failure: Option<bool>,
  #[serde(alias = "per_profile_inputs")]
  per_profile_inputs: Option<HashMap<String, serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpRecipe {
  version: u32,
  name: String,
  description: Option<String>,
  defaults: Option<McpRecipeDefaults>,
  inputs: Option<serde_json::Value>,
  #[serde(default)]
  steps: Vec<McpRecipeStep>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpRecipeDefaults {
  #[serde(alias = "timeout_ms", alias = "timeoutMs")]
  timeout_ms: Option<u64>,
  #[serde(alias = "auto_wait", alias = "autoWait")]
  auto_wait: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum McpRecipeOnError {
  #[default]
  Stop,
  Continue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpRecipeRetry {
  #[serde(alias = "max_attempts", alias = "maxAttempts")]
  max_attempts: u32,
  #[serde(alias = "delay_ms", alias = "delayMs")]
  delay_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpRecipeStep {
  id: String,
  tool: String,
  #[serde(default = "default_recipe_step_arguments")]
  arguments: serde_json::Value,
  condition: Option<serde_json::Value>,
  output: Option<String>,
  #[serde(alias = "on_error", default)]
  on_error: McpRecipeOnError,
  retry: Option<McpRecipeRetry>,
}

fn default_recipe_step_arguments() -> serde_json::Value {
  serde_json::json!({})
}

#[derive(Debug, Deserialize)]
struct ReadProfileCookiesArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
}

#[derive(Debug, Deserialize)]
struct McpSelectedCookie {
  domain: String,
  name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CopyProfileCookiesArgs {
  #[serde(alias = "sourceProfileId")]
  source_profile_id: String,
  #[serde(alias = "targetProfileIds")]
  target_profile_ids: Vec<String>,
  #[serde(alias = "selectedCookies", default)]
  selected_cookies: Vec<McpSelectedCookie>,
}

#[derive(Debug, Deserialize)]
struct ImportCookiesFromFileArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  content: String,
}

#[derive(Debug, Deserialize)]
struct ExportProfileCookiesArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  format: String,
}

#[derive(Debug, Deserialize)]
struct GetStorageArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SetStorageArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  key: String,
  value: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct ClearStorageArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  storage: Option<String>,
  key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GenerateSampleFingerprintArgs {
  browser: String,
  version: String,
  config: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct WaitForSelectorArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  locator: McpLocator,
  frame: Option<McpFrameRef>,
  state: Option<String>,
  #[serde(flatten)]
  options: McpCommonOptions,
}

#[derive(Debug, Deserialize)]
struct WaitForTextArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  text: String,
  selector: Option<String>,
  locator: Option<McpLocator>,
  exact: Option<bool>,
  #[serde(flatten)]
  options: McpCommonOptions,
}

#[derive(Debug, Deserialize)]
struct WaitForNavigationArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  #[serde(alias = "fromUrl")]
  from_url: Option<String>,
  #[serde(flatten)]
  options: McpCommonOptions,
}

#[derive(Debug, Deserialize)]
struct FrameTreeArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
}

#[derive(Debug, Deserialize)]
struct TabTreeArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
}

#[derive(Debug, Deserialize)]
struct SwitchTabArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  tab: McpTabRef,
}

#[derive(Debug, Deserialize)]
struct NewTabArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CloseTabArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  tab: Option<McpTabRef>,
}

#[derive(Debug, Deserialize)]
struct FileInputArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  selector: Option<String>,
  locator: Option<McpLocator>,
  frame: Option<McpFrameRef>,
  files: Vec<String>,
  #[serde(flatten)]
  options: McpCommonOptions,
}

#[derive(Debug, Deserialize)]
struct FindFrameArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  frame: McpFrameRef,
}

#[derive(Debug, Deserialize)]
struct EvaluateJavascriptArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  expression: String,
  frame: Option<McpFrameRef>,
  await_promise: Option<bool>,
  wait_for_load: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ElementInteractionArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  selector: Option<String>,
  locator: Option<McpLocator>,
  frame: Option<McpFrameRef>,
  #[serde(flatten)]
  options: McpCommonOptions,
}

#[derive(Debug, Deserialize)]
struct ClickElementArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  selector: Option<String>,
  locator: Option<McpLocator>,
  frame: Option<McpFrameRef>,
  #[serde(flatten)]
  options: McpCommonOptions,
}

#[derive(Debug, Deserialize)]
struct TypeTextArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  selector: Option<String>,
  locator: Option<McpLocator>,
  frame: Option<McpFrameRef>,
  text: String,
  clear_first: Option<bool>,
  instant: Option<bool>,
  wpm: Option<f64>,
  #[serde(flatten)]
  options: McpCommonOptions,
}

#[derive(Debug, Deserialize)]
struct GetPageContentArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  format: Option<String>,
  selector: Option<String>,
  frame: Option<McpFrameRef>,
}

#[derive(Debug, Deserialize)]
struct ScrollToArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  x: Option<f64>,
  y: Option<f64>,
  behavior: Option<String>,
  frame: Option<McpFrameRef>,
}

#[derive(Debug, Deserialize)]
struct PressKeyArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  key: String,
  code: Option<String>,
  text: Option<String>,
  frame: Option<McpFrameRef>,
}

#[derive(Debug, Deserialize)]
struct PressHotkeyArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  keys: Vec<String>,
  frame: Option<McpFrameRef>,
}

#[derive(Debug, Deserialize)]
struct SelectOptionArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  selector: Option<String>,
  locator: Option<McpLocator>,
  frame: Option<McpFrameRef>,
  value: Option<String>,
  label: Option<String>,
  index: Option<usize>,
  #[serde(flatten)]
  options: McpCommonOptions,
}

#[derive(Debug, Deserialize)]
struct FrameLocatorReadArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  locator: McpLocator,
  frame: Option<McpFrameRef>,
  #[serde(flatten)]
  options: McpCommonOptions,
}

#[derive(Debug, Deserialize)]
struct GetElementAttributeArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  locator: McpLocator,
  frame: Option<McpFrameRef>,
  name: String,
  #[serde(flatten)]
  options: McpCommonOptions,
}

#[derive(Debug, Deserialize)]
struct LocatorReadArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  locator: McpLocator,
  frame: Option<McpFrameRef>,
  #[serde(flatten)]
  options: McpCommonOptions,
}

#[derive(Debug, Deserialize)]
struct QueryElementsArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  locator: McpLocator,
  frame: Option<McpFrameRef>,
  limit: Option<usize>,
  #[serde(flatten)]
  options: McpCommonOptions,
}

#[derive(Debug, Deserialize)]
struct DownloadFileArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  url: Option<String>,
  #[serde(alias = "fileName")]
  file_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WaitForDownloadArgs {
  #[serde(alias = "profileId")]
  profile_id: String,
  #[serde(alias = "downloadId")]
  download_id: String,
  #[serde(flatten)]
  options: McpCommonOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct NavigationSnapshot {
  url: String,
  title: String,
  ready_state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct McpTabNode {
  id: String,
  index: usize,
  title: String,
  url: String,
  #[serde(rename = "type")]
  target_type: String,
  active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CdpPageTarget {
  id: String,
  title: String,
  url: String,
  target_type: String,
  web_socket_debugger_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConsoleLogEntry {
  timestamp: Option<f64>,
  source: String,
  level: String,
  text: String,
  url: Option<String>,
  line_number: Option<i64>,
}

#[derive(Clone)]
struct ConsoleCaptureShared {
  logs: Arc<AsyncMutex<VecDeque<ConsoleLogEntry>>>,
  running: Arc<AtomicBool>,
  last_error: Arc<AsyncMutex<Option<String>>>,
}

struct ConsoleCaptureState {
  shared: ConsoleCaptureShared,
  shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct NetworkRequestEntry {
  request_id: String,
  url: String,
  method: String,
  resource_type: Option<String>,
  initiator_type: Option<String>,
  start_timestamp: Option<f64>,
  end_timestamp: Option<f64>,
  duration_ms: Option<f64>,
  status: Option<i64>,
  status_text: Option<String>,
  mime_type: Option<String>,
  request_headers: serde_json::Value,
  response_headers: serde_json::Value,
  post_data: Option<String>,
  encoded_data_length: Option<f64>,
  failed: bool,
  error_text: Option<String>,
  blocked_reason: Option<String>,
  has_response_body: bool,
  #[serde(skip_serializing)]
  response_body: Option<String>,
  #[serde(skip_serializing)]
  response_body_base64_encoded: bool,
}

impl NetworkRequestEntry {
  fn new(request_id: &str) -> Self {
    Self {
      request_id: request_id.to_string(),
      url: String::new(),
      method: String::new(),
      resource_type: None,
      initiator_type: None,
      start_timestamp: None,
      end_timestamp: None,
      duration_ms: None,
      status: None,
      status_text: None,
      mime_type: None,
      request_headers: serde_json::json!({}),
      response_headers: serde_json::json!({}),
      post_data: None,
      encoded_data_length: None,
      failed: false,
      error_text: None,
      blocked_reason: None,
      has_response_body: false,
      response_body: None,
      response_body_base64_encoded: false,
    }
  }
}

#[derive(Clone)]
struct NetworkCaptureShared {
  requests: Arc<AsyncMutex<VecDeque<NetworkRequestEntry>>>,
  running: Arc<AtomicBool>,
  last_error: Arc<AsyncMutex<Option<String>>>,
}

struct BodyFetchRequest {
  request_id: String,
  response_tx: tokio::sync::oneshot::Sender<Result<(String, bool), String>>,
}

type PendingBodyFetchSender = tokio::sync::oneshot::Sender<Result<(String, bool), String>>;
type PendingBodyFetchMap = HashMap<u64, (String, PendingBodyFetchSender)>;

struct NetworkCaptureState {
  shared: NetworkCaptureShared,
  shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
  body_request_tx: mpsc::Sender<BodyFetchRequest>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadRecord {
  download_id: String,
  profile_id: String,
  url: String,
  file_name: String,
  file_path: String,
  status: String,
  bytes_written: Option<u64>,
  error: Option<String>,
}

#[derive(Clone)]
struct DownloadState {
  record: Arc<AsyncMutex<DownloadRecord>>,
}

struct McpSession {
  initialized: bool,
}

struct McpServerInner {
  app_handle: Option<AppHandle>,
  token: Option<String>,
  shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
  sessions: HashMap<String, McpSession>,
  selected_tabs: HashMap<u16, String>,
  console_captures: HashMap<String, ConsoleCaptureState>,
  network_captures: HashMap<String, NetworkCaptureState>,
  downloads: HashMap<String, DownloadState>,
}

#[derive(Clone)]
struct McpHttpState {
  server: &'static McpServer,
  token: String,
}

pub struct McpServer {
  inner: Arc<AsyncMutex<McpServerInner>>,
  is_running: AtomicBool,
  port: AtomicU16,
}

impl McpServer {
  fn new() -> Self {
    Self {
      inner: Arc::new(AsyncMutex::new(McpServerInner {
        app_handle: None,
        token: None,
        shutdown_tx: None,
        sessions: HashMap::new(),
        selected_tabs: HashMap::new(),
        console_captures: HashMap::new(),
        network_captures: HashMap::new(),
        downloads: HashMap::new(),
      })),
      is_running: AtomicBool::new(false),
      port: AtomicU16::new(0),
    }
  }

  pub fn instance() -> &'static McpServer {
    &MCP_SERVER
  }

  pub fn is_running(&self) -> bool {
    self.is_running.load(Ordering::SeqCst)
  }

  async fn require_paid_subscription(_feature: &str) -> Result<(), McpError> {
    Ok(())
  }

  async fn get_app_handle(&self) -> Result<AppHandle, McpError> {
    let inner = self.inner.lock().await;
    inner
      .app_handle
      .clone()
      .ok_or_else(|| McpError::internal("MCP server not properly initialized"))
  }

  fn text_tool_result(text: impl Into<String>) -> serde_json::Value {
    serde_json::json!({
      "content": [{
        "type": "text",
        "text": text.into()
      }]
    })
  }

  fn json_tool_result<T: Serialize>(value: &T) -> Result<serde_json::Value, McpError> {
    let text = serde_json::to_string_pretty(value)
      .map_err(|e| McpError::internal(format!("Failed to serialize MCP tool result: {e}")))?;
    Ok(Self::text_tool_result(text))
  }

  fn parse_arguments<T: DeserializeOwned>(arguments: &serde_json::Value) -> Result<T, McpError> {
    serde_json::from_value(arguments.clone())
      .map_err(|e| McpError::invalid_params(format!("Invalid arguments: {e}")))
  }

  fn snake_to_camel_case(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut uppercase_next = false;
    for character in value.chars() {
      if character == '_' {
        uppercase_next = true;
        continue;
      }

      if uppercase_next {
        result.extend(character.to_uppercase());
        uppercase_next = false;
      } else {
        result.push(character);
      }
    }
    result
  }

  fn argument_key_present(
    arguments: &serde_json::Map<String, serde_json::Value>,
    key: &str,
  ) -> bool {
    arguments.contains_key(key) || arguments.contains_key(&Self::snake_to_camel_case(key))
  }

  fn validate_tool_arguments_shape(
    arguments: &serde_json::Value,
    schema: &serde_json::Value,
    tool_name: &str,
  ) -> Result<(), McpError> {
    let object = arguments.as_object().ok_or_else(|| {
      McpError::invalid_params(format!(
        "Recipe step for tool {tool_name} must provide arguments as an object"
      ))
    })?;

    let missing_required = schema
      .get("required")
      .and_then(|value| value.as_array())
      .into_iter()
      .flatten()
      .filter_map(|value| value.as_str())
      .filter(|field| !Self::argument_key_present(object, field))
      .map(ToString::to_string)
      .collect::<Vec<_>>();

    if !missing_required.is_empty() {
      return Err(McpError::invalid_params(format!(
        "Recipe step for tool {tool_name} is missing required argument(s): {}",
        missing_required.join(", ")
      )));
    }

    if let Some(any_of) = schema.get("anyOf").and_then(|value| value.as_array()) {
      let satisfies_any = any_of.iter().any(|branch| {
        branch
          .get("required")
          .and_then(|value| value.as_array())
          .map(|required| {
            required
              .iter()
              .filter_map(|value| value.as_str())
              .all(|field| Self::argument_key_present(object, field))
          })
          .unwrap_or(false)
      });

      if !satisfies_any {
        let alternatives = any_of
          .iter()
          .filter_map(|branch| branch.get("required").and_then(|value| value.as_array()))
          .map(|required| {
            required
              .iter()
              .filter_map(|value| value.as_str())
              .collect::<Vec<_>>()
              .join(" + ")
          })
          .filter(|value| !value.is_empty())
          .collect::<Vec<_>>();

        if !alternatives.is_empty() {
          return Err(McpError::invalid_params(format!(
            "Recipe step for tool {tool_name} must satisfy one of these argument sets: {}",
            alternatives.join(" | ")
          )));
        }
      }
    }

    Ok(())
  }

  fn recipe_retry_is_safe(tool_name: &str) -> bool {
    matches!(
      tool_name,
      "wait_for_selector"
        | "wait_for_text"
        | "wait_for_navigation"
        | "element_exists"
        | "get_element_text"
        | "get_element_html"
        | "get_element_attribute"
        | "get_element_rect"
        | "query_elements"
        | "count_elements"
        | "get_page_content"
        | "get_page_info"
        | "list_tabs"
        | "get_active_tab"
        | "list_frames"
        | "get_frame_tree"
        | "find_frame"
        | "get_console_logs"
        | "get_network_requests"
        | "get_response_body"
        | "wait_for_download"
        | "read_profile_cookies"
        | "get_local_storage"
        | "get_session_storage"
        | "generate_sample_fingerprint"
        | "is_geoip_database_available"
        | "get_profile_status"
    )
  }

  fn recipe_reserved_tool(tool_name: &str) -> bool {
    matches!(
      tool_name,
      "validate_recipe" | "preview_recipe" | "run_recipe" | "run_batch_profile_workflow"
    )
  }

  fn validate_recipe_definition(
    &self,
    recipe: &McpRecipe,
  ) -> Result<Vec<serde_json::Value>, McpError> {
    if recipe.version != 1 {
      return Err(McpError::invalid_params(format!(
        "Unsupported recipe version {}. Only version 1 is supported",
        recipe.version
      )));
    }

    Self::validate_non_empty_key(&recipe.name, "recipe.name")?;

    if recipe.steps.is_empty() {
      return Err(McpError::invalid_params(
        "Recipe must contain at least one step",
      ));
    }

    let tools = self
      .get_tools()
      .into_iter()
      .map(|tool| (tool.name, tool.input_schema))
      .collect::<HashMap<_, _>>();
    let mut seen_step_ids = std::collections::HashSet::new();
    let mut seen_outputs = std::collections::HashSet::new();
    let mut warnings = Vec::new();

    for (index, step) in recipe.steps.iter().enumerate() {
      Self::validate_non_empty_key(&step.id, "step.id")?;
      Self::validate_non_empty_key(&step.tool, "step.tool")?;

      if !seen_step_ids.insert(step.id.clone()) {
        return Err(McpError::invalid_params(format!(
          "Duplicate recipe step id: {}",
          step.id
        )));
      }

      if let Some(output) = step.output.as_deref() {
        Self::validate_non_empty_key(output, "step.output")?;
        if !seen_outputs.insert(output.to_string()) {
          return Err(McpError::invalid_params(format!(
            "Duplicate recipe output name: {output}"
          )));
        }
      }

      if Self::recipe_reserved_tool(&step.tool) {
        return Err(McpError::invalid_params(format!(
          "Recipe step {} cannot call reserved orchestration tool {}",
          step.id, step.tool
        )));
      }

      let Some(schema) = tools.get(&step.tool) else {
        return Err(McpError::invalid_params(format!(
          "Recipe step {} references unknown tool {}",
          step.id, step.tool
        )));
      };

      Self::validate_tool_arguments_shape(&step.arguments, schema, &step.tool)?;

      if let Some(retry) = &step.retry {
        if retry.max_attempts == 0 {
          return Err(McpError::invalid_params(format!(
            "Recipe step {} retry.max_attempts must be at least 1",
            step.id
          )));
        }

        if !Self::recipe_retry_is_safe(&step.tool) {
          warnings.push(serde_json::json!({
            "code": "unsafe_retry_tool",
            "message": format!("Step {} uses retry on tool {}. This tool is treated as stateful, so retry should be reviewed manually.", step.id, step.tool),
            "stepId": step.id,
            "tool": step.tool,
            "index": index,
          }));
        }
      }
    }

    Ok(warnings)
  }

  fn apply_recipe_defaults(
    arguments: &serde_json::Value,
    defaults: Option<&McpRecipeDefaults>,
  ) -> serde_json::Value {
    let Some(defaults) = defaults else {
      return arguments.clone();
    };
    let Some(object) = arguments.as_object() else {
      return arguments.clone();
    };

    let mut merged = object.clone();
    if let Some(timeout_ms) = defaults.timeout_ms {
      if !Self::argument_key_present(&merged, "timeout_ms") {
        merged.insert("timeout_ms".to_string(), serde_json::json!(timeout_ms));
      }
    }
    if let Some(auto_wait) = defaults.auto_wait {
      if !Self::argument_key_present(&merged, "auto_wait") {
        merged.insert("auto_wait".to_string(), serde_json::json!(auto_wait));
      }
    }

    serde_json::Value::Object(merged)
  }

  fn normalize_recipe_inputs(inputs: Option<serde_json::Value>) -> serde_json::Value {
    match inputs {
      Some(serde_json::Value::Object(map)) => serde_json::Value::Object(map),
      Some(_) => serde_json::json!({}),
      None => serde_json::json!({}),
    }
  }

  fn merge_recipe_input_objects(
    base_inputs: &serde_json::Value,
    override_inputs: Option<&serde_json::Value>,
  ) -> serde_json::Value {
    let mut merged = match base_inputs {
      serde_json::Value::Object(map) => map.clone(),
      _ => serde_json::Map::new(),
    };

    if let Some(serde_json::Value::Object(map)) = override_inputs {
      for (key, value) in map {
        merged.insert(key.clone(), value.clone());
      }
    }

    serde_json::Value::Object(merged)
  }

  fn build_profile_recipe_inputs(
    base_inputs: &serde_json::Value,
    per_profile_inputs: Option<&HashMap<String, serde_json::Value>>,
    profile_id: &str,
  ) -> serde_json::Value {
    let override_inputs = per_profile_inputs.and_then(|values| values.get(profile_id));
    let mut merged = match Self::merge_recipe_input_objects(base_inputs, override_inputs) {
      serde_json::Value::Object(map) => map,
      _ => serde_json::Map::new(),
    };

    merged.insert(
      "profile_id".to_string(),
      serde_json::Value::String(profile_id.to_string()),
    );
    merged.insert(
      "profileId".to_string(),
      serde_json::Value::String(profile_id.to_string()),
    );

    serde_json::Value::Object(merged)
  }

  fn normalize_recipe_step_result(value: &serde_json::Value) -> serde_json::Value {
    let text = value
      .get("content")
      .and_then(|content| content.as_array())
      .and_then(|content| content.first())
      .and_then(|item| item.get("text"))
      .and_then(|text| text.as_str());

    if let Some(text) = text {
      serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.to_string()))
    } else {
      value.clone()
    }
  }

  fn resolve_recipe_path(
    path: &str,
    inputs: &serde_json::Value,
    steps: &serde_json::Map<String, serde_json::Value>,
  ) -> Option<serde_json::Value> {
    let mut parts = path.split('.');
    match parts.next()? {
      "inputs" => {
        let mut current = inputs;
        for part in parts {
          current = current.get(part)?;
        }
        Some(current.clone())
      }
      "steps" => {
        let step_id = parts.next()?;
        let mut current = steps.get(step_id)?;
        for part in parts {
          current = current.get(part)?;
        }
        Some(current.clone())
      }
      _ => None,
    }
  }

  fn recipe_value_to_inline_string(value: &serde_json::Value) -> String {
    match value {
      serde_json::Value::Null => "null".to_string(),
      serde_json::Value::Bool(boolean) => boolean.to_string(),
      serde_json::Value::Number(number) => number.to_string(),
      serde_json::Value::String(text) => text.clone(),
      _ => serde_json::to_string(value).unwrap_or_else(|_| String::new()),
    }
  }

  fn resolve_recipe_string(
    value: &str,
    inputs: &serde_json::Value,
    steps: &serde_json::Map<String, serde_json::Value>,
  ) -> Result<serde_json::Value, McpError> {
    let trimmed = value.trim();
    if trimmed.starts_with("{{") && trimmed.ends_with("}}") && trimmed.matches("{{").count() == 1 {
      let path = trimmed
        .trim_start_matches("{{")
        .trim_end_matches("}}")
        .trim();
      return Self::resolve_recipe_path(path, inputs, steps).ok_or_else(|| {
        McpError::invalid_params(format!("Recipe placeholder could not be resolved: {path}"))
      });
    }

    let mut rendered = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("{{") {
      let (prefix, suffix) = rest.split_at(start);
      rendered.push_str(prefix);
      let Some(end) = suffix.find("}}") else {
        return Err(McpError::invalid_params(format!(
          "Recipe placeholder is missing closing braces in: {value}"
        )));
      };
      let placeholder = &suffix[2..end].trim();
      let resolved = Self::resolve_recipe_path(placeholder, inputs, steps).ok_or_else(|| {
        McpError::invalid_params(format!(
          "Recipe placeholder could not be resolved: {placeholder}"
        ))
      })?;
      rendered.push_str(&Self::recipe_value_to_inline_string(&resolved));
      rest = &suffix[end + 2..];
    }
    rendered.push_str(rest);
    Ok(serde_json::Value::String(rendered))
  }

  fn resolve_recipe_value(
    value: &serde_json::Value,
    inputs: &serde_json::Value,
    steps: &serde_json::Map<String, serde_json::Value>,
  ) -> Result<serde_json::Value, McpError> {
    match value {
      serde_json::Value::Object(map) => {
        let mut resolved = serde_json::Map::with_capacity(map.len());
        for (key, value) in map {
          resolved.insert(
            key.clone(),
            Self::resolve_recipe_value(value, inputs, steps)?,
          );
        }
        Ok(serde_json::Value::Object(resolved))
      }
      serde_json::Value::Array(values) => {
        let mut resolved = Vec::with_capacity(values.len());
        for value in values {
          resolved.push(Self::resolve_recipe_value(value, inputs, steps)?);
        }
        Ok(serde_json::Value::Array(resolved))
      }
      serde_json::Value::String(text) => Self::resolve_recipe_string(text, inputs, steps),
      _ => Ok(value.clone()),
    }
  }

  fn evaluate_recipe_condition(
    condition: Option<&serde_json::Value>,
    inputs: &serde_json::Value,
    steps: &serde_json::Map<String, serde_json::Value>,
  ) -> Result<(bool, serde_json::Value), McpError> {
    let Some(condition) = condition else {
      return Ok((
        true,
        serde_json::json!({ "provided": false, "matched": true }),
      ));
    };

    match condition {
      serde_json::Value::Bool(value) => Ok((
        *value,
        serde_json::json!({
          "provided": true,
          "type": "boolean",
          "matched": value,
        }),
      )),
      serde_json::Value::String(path) => {
        let resolved = Self::resolve_recipe_path(path, inputs, steps).ok_or_else(|| {
          McpError::invalid_params(format!(
            "Recipe condition path could not be resolved: {path}"
          ))
        })?;
        let matched = match &resolved {
          serde_json::Value::Null => false,
          serde_json::Value::Bool(value) => *value,
          serde_json::Value::Number(number) => number.as_i64().unwrap_or(1) != 0,
          serde_json::Value::String(text) => !text.trim().is_empty(),
          serde_json::Value::Array(values) => !values.is_empty(),
          serde_json::Value::Object(map) => !map.is_empty(),
        };
        Ok((
          matched,
          serde_json::json!({
            "provided": true,
            "type": "path",
            "path": path,
            "resolved": resolved,
            "matched": matched,
          }),
        ))
      }
      serde_json::Value::Object(map) => {
        let path = map
          .get("path")
          .and_then(|value| value.as_str())
          .ok_or_else(|| {
            McpError::invalid_params(
              "Recipe condition object must include a string field named path",
            )
          })?;
        let resolved = Self::resolve_recipe_path(path, inputs, steps).ok_or_else(|| {
          McpError::invalid_params(format!(
            "Recipe condition path could not be resolved: {path}"
          ))
        })?;

        if let Some(exists) = map.get("exists").and_then(|value| value.as_bool()) {
          let matched = exists != resolved.is_null();
          return Ok((
            matched,
            serde_json::json!({
              "provided": true,
              "type": "exists",
              "path": path,
              "resolved": resolved,
              "expected": exists,
              "matched": matched,
            }),
          ));
        }

        if let Some(expected) = map.get("equals") {
          let matched = resolved == *expected;
          return Ok((
            matched,
            serde_json::json!({
              "provided": true,
              "type": "equals",
              "path": path,
              "resolved": resolved,
              "expected": expected,
              "matched": matched,
            }),
          ));
        }

        Err(McpError::invalid_params(
          "Recipe condition object must include either exists or equals",
        ))
      }
      _ => Err(McpError::invalid_params(
        "Recipe condition must be a boolean, string path, or object rule",
      )),
    }
  }

  fn prepare_storage_value(value: &serde_json::Value) -> Result<(String, &'static str), McpError> {
    if let Some(text) = value.as_str() {
      return Ok((text.to_string(), "string"));
    }

    serde_json::to_string(value)
      .map(|serialized| (serialized, "json"))
      .map_err(|e| McpError::internal(format!("Failed to serialize storage value: {e}")))
  }

  fn remote_object_to_text(remote: &serde_json::Value) -> String {
    if let Some(value) = remote.get("value") {
      return if let Some(text) = value.as_str() {
        text.to_string()
      } else {
        value.to_string()
      };
    }

    if let Some(text) = remote.get("unserializableValue").and_then(|v| v.as_str()) {
      return text.to_string();
    }

    if let Some(text) = remote.get("description").and_then(|v| v.as_str()) {
      return text.to_string();
    }

    remote
      .get("type")
      .and_then(|v| v.as_str())
      .map(|value_type| format!("[{value_type}]"))
      .unwrap_or_else(|| "[unknown]".to_string())
  }

  fn console_level_from_runtime(kind: &str) -> String {
    match kind {
      "warning" => "warning",
      "error" => "error",
      "debug" => "debug",
      "info" => "info",
      _ => "log",
    }
    .to_string()
  }

  async fn push_console_log(shared: &ConsoleCaptureShared, entry: ConsoleLogEntry) {
    let mut logs = shared.logs.lock().await;
    logs.push_back(entry);
    while logs.len() > MAX_CONSOLE_LOG_ENTRIES {
      logs.pop_front();
    }
  }

  async fn set_console_capture_error(shared: &ConsoleCaptureShared, message: impl Into<String>) {
    shared.running.store(false, Ordering::SeqCst);
    let mut last_error = shared.last_error.lock().await;
    *last_error = Some(message.into());
  }

  async fn update_network_request<F>(shared: &NetworkCaptureShared, request_id: &str, updater: F)
  where
    F: FnOnce(&mut NetworkRequestEntry),
  {
    let mut requests = shared.requests.lock().await;
    if let Some(position) = requests
      .iter()
      .position(|entry| entry.request_id == request_id)
    {
      if let Some(entry) = requests.get_mut(position) {
        updater(entry);
      }
      return;
    }

    let mut entry = NetworkRequestEntry::new(request_id);
    updater(&mut entry);
    requests.push_back(entry);
    while requests.len() > MAX_NETWORK_REQUEST_ENTRIES {
      requests.pop_front();
    }
  }

  async fn set_network_request_body(
    shared: &NetworkCaptureShared,
    request_id: &str,
    body: String,
    base64_encoded: bool,
  ) {
    Self::update_network_request(shared, request_id, move |entry| {
      entry.has_response_body = true;
      entry.response_body = Some(body);
      entry.response_body_base64_encoded = base64_encoded;
    })
    .await;
  }

  async fn fail_pending_body_requests(
    pending_requests: &mut PendingBodyFetchMap,
    message: impl Into<String>,
  ) {
    let message = message.into();
    for (request_id, response_tx) in pending_requests.drain().map(|(_, value)| value) {
      let _ = response_tx.send(Err(format!(
        "Response body is unavailable for request {request_id}: {message}"
      )));
    }
  }

  async fn set_network_capture_error(shared: &NetworkCaptureShared, message: impl Into<String>) {
    shared.running.store(false, Ordering::SeqCst);
    let mut last_error = shared.last_error.lock().await;
    *last_error = Some(message.into());
  }

  fn notify_capture_ready(
    ready_tx: &mut Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
    result: Result<(), String>,
  ) {
    if let Some(tx) = ready_tx.take() {
      let _ = tx.send(result);
    }
  }

  async fn queue_sync_for_profiles_if_enabled(profile_ids: &[String]) {
    let Some(scheduler) = crate::sync::get_global_scheduler() else {
      return;
    };

    let Ok(profiles) = ProfileManager::instance().list_profiles() else {
      return;
    };

    let sync_ids: Vec<String> = profile_ids
      .iter()
      .filter(|profile_id| {
        profiles
          .iter()
          .any(|profile| profile.id.to_string() == **profile_id && profile.is_sync_enabled())
      })
      .cloned()
      .collect();

    if sync_ids.is_empty() {
      return;
    }

    tauri::async_runtime::spawn(async move {
      for profile_id in sync_ids {
        scheduler.queue_profile_sync(profile_id).await;
      }
    });
  }

  async fn evaluate_javascript_value(
    &self,
    profile_id: &str,
    expression: &str,
  ) -> Result<serde_json::Value, McpError> {
    let profile = self.get_running_profile(profile_id)?;
    if profile.browser == "camoufox" {
      let page = self.get_camoufox_active_page(&profile).await?;
      return self.evaluate_camoufox_page_value(&page, expression).await;
    }
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    crate::chromium_manager::ChromiumManager::instance()
      .refresh_runtime_fingerprint_overrides_for_target(&profile, &ws_url)
      .await
      .map_err(|error| {
        McpError::internal(format!(
          "Failed to refresh Chromium runtime fingerprint overrides: {error}"
        ))
      })?;
    self.evaluate_runtime_value(&ws_url, expression, None).await
  }

  async fn evaluate_runtime_value(
    &self,
    ws_url: &str,
    expression: &str,
    context_id: Option<i64>,
  ) -> Result<serde_json::Value, McpError> {
    let mut params = serde_json::json!({
      "expression": expression,
      "returnByValue": true,
    });
    if let Some(context_id) = context_id {
      params["contextId"] = serde_json::json!(context_id);
    }

    let result = self.send_cdp(ws_url, "Runtime.evaluate", params).await?;

    if let Some(exception) = result.get("exceptionDetails") {
      let message = exception
        .get("text")
        .or_else(|| {
          exception
            .get("exception")
            .and_then(|exception| exception.get("description"))
        })
        .and_then(|value| value.as_str())
        .unwrap_or("JavaScript evaluation failed");
      return Err(McpError::internal(message));
    }

    Ok(
      result
        .get("result")
        .and_then(|value| value.get("value"))
        .cloned()
        .unwrap_or(serde_json::Value::Null),
    )
  }

  async fn evaluate_runtime_object_id(
    &self,
    ws_url: &str,
    expression: &str,
    context_id: Option<i64>,
  ) -> Result<Option<String>, McpError> {
    let mut params = serde_json::json!({
      "expression": expression,
      "returnByValue": false,
    });
    if let Some(context_id) = context_id {
      params["contextId"] = serde_json::json!(context_id);
    }

    let result = self.send_cdp(ws_url, "Runtime.evaluate", params).await?;
    if let Some(exception) = result.get("exceptionDetails") {
      let message = exception
        .get("text")
        .or_else(|| {
          exception
            .get("exception")
            .and_then(|exception| exception.get("description"))
        })
        .and_then(|value| value.as_str())
        .unwrap_or("JavaScript evaluation failed");
      return Err(McpError::internal(message));
    }

    Ok(
      result
        .get("result")
        .and_then(|value| value.get("objectId"))
        .and_then(|value| value.as_str())
        .map(ToString::to_string),
    )
  }

  async fn execute_runtime_expression(
    &self,
    ws_url: &str,
    expression: &str,
    context_id: Option<i64>,
    wait_for_load: bool,
  ) -> Result<serde_json::Value, McpError> {
    let mut params = serde_json::json!({
      "expression": expression,
      "returnByValue": true,
    });
    if let Some(context_id) = context_id {
      params["contextId"] = serde_json::json!(context_id);
    }

    if wait_for_load {
      self
        .send_cdp_and_wait_for_load(ws_url, "Runtime.evaluate", params, 10)
        .await
    } else {
      self.send_cdp(ws_url, "Runtime.evaluate", params).await
    }
  }

  fn runtime_result_value(result: &serde_json::Value) -> serde_json::Value {
    result
      .get("result")
      .and_then(|value| value.get("value"))
      .cloned()
      .unwrap_or(serde_json::Value::Null)
  }

  async fn wait_with_timeout<T, F, Fut>(
    &self,
    timeout_ms: u64,
    mut condition: F,
    mut timeout_message: impl FnMut() -> String,
  ) -> Result<T, McpError>
  where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Option<T>, McpError>>,
  {
    let deadline =
      tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms.max(1_000));

    loop {
      if let Some(value) = condition().await? {
        return Ok(value);
      }

      let now = tokio::time::Instant::now();
      if now >= deadline {
        return Err(McpError::timeout(timeout_message()));
      }

      let remaining = deadline.saturating_duration_since(now);
      tokio::time::sleep(remaining.min(std::time::Duration::from_millis(WAIT_POLL_INTERVAL_MS)))
        .await;
    }
  }

  fn locator_helpers_script() -> &'static str {
    r#"
        const isVisible = (element) => {
          if (!element || !(element instanceof Element)) return false;
          const style = window.getComputedStyle(element);
          if (style.display === "none" || style.visibility === "hidden" || style.opacity === "0") {
            return false;
          }
          const rect = element.getBoundingClientRect();
          return rect.width > 0 && rect.height > 0;
        };

        const getInnerText = (node) => {
          if (!node) return "";
          return String(node.innerText || node.textContent || "").trim();
        };

        const getLabelTextForControl = (element) => {
          if (!(element instanceof Element)) return "";
          const labels = Array.from(element.labels || []);
          if (labels.length > 0) {
            return labels.map((label) => getInnerText(label)).filter(Boolean).join(" ").trim();
          }
          const id = element.getAttribute("id");
          if (!id) return "";
          return Array.from(document.querySelectorAll(`label[for="${CSS.escape(id)}"]`))
            .map((label) => getInnerText(label))
            .filter(Boolean)
            .join(" ")
            .trim();
        };

        const getAriaLabelledText = (element) => {
          if (!(element instanceof Element)) return "";
          const labelledBy = element.getAttribute("aria-labelledby");
          if (!labelledBy) return "";
          return labelledBy
            .split(/\s+/)
            .map((id) => document.getElementById(id))
            .filter(Boolean)
            .map((node) => getInnerText(node))
            .filter(Boolean)
            .join(" ")
            .trim();
        };

        const getElementLabelText = (element) => {
          if (!(element instanceof Element)) return "";
          const ariaLabel = element.getAttribute("aria-label");
          if (ariaLabel && ariaLabel.trim()) return ariaLabel.trim();

          const labelledByText = getAriaLabelledText(element);
          if (labelledByText) return labelledByText;

          return getLabelTextForControl(element);
        };

        const getElementAccessibleName = (element) => {
          if (!(element instanceof Element)) return "";
          const ariaLabel = element.getAttribute("aria-label");
          if (ariaLabel && ariaLabel.trim()) return ariaLabel.trim();

          const labelledByText = getAriaLabelledText(element);
          if (labelledByText) return labelledByText;

          const labelText = getLabelTextForControl(element);
          if (labelText) return labelText;

          if (element instanceof HTMLInputElement) {
            const type = String(element.getAttribute("type") || "").toLowerCase();
            if (type === "button" || type === "submit" || type === "reset") {
              if (element.value && element.value.trim()) return element.value.trim();
            }
            if (type === "image") {
              const alt = element.getAttribute("alt");
              if (alt && alt.trim()) return alt.trim();
            }
          }

          if (element instanceof HTMLImageElement) {
            const alt = element.getAttribute("alt");
            if (alt && alt.trim()) return alt.trim();
          }

          const title = element.getAttribute("title");
          if (title && title.trim()) return title.trim();

          const placeholder = element.getAttribute("placeholder");
          if (placeholder && placeholder.trim()) return placeholder.trim();

          return getInnerText(element);
        };

        const getImplicitRole = (element) => {
          if (!(element instanceof Element)) return null;
          const tag = element.tagName.toLowerCase();
          if (tag === "button") return "button";
          if (tag === "a" && element.hasAttribute("href")) return "link";
          if (tag === "select") return element.hasAttribute("multiple") ? "listbox" : "combobox";
          if (tag === "textarea") return "textbox";
          if (tag === "summary") return "button";
          if (tag === "img" && element.getAttribute("alt") !== "") return "img";
          if (tag === "input") {
            const type = String(element.getAttribute("type") || "text").toLowerCase();
            switch (type) {
              case "button":
              case "submit":
              case "reset":
                return "button";
              case "checkbox":
                return "checkbox";
              case "radio":
                return "radio";
              case "range":
                return "slider";
              case "email":
              case "search":
              case "tel":
              case "text":
              case "url":
              case "password":
                return "textbox";
              default:
                return null;
            }
          }
          return null;
        };

        const getElementRole = (element) => {
          if (!(element instanceof Element)) return null;
          const explicitRole = element.getAttribute("role");
          if (explicitRole && explicitRole.trim()) {
            return explicitRole.trim().split(/\s+/)[0].toLowerCase();
          }
          return getImplicitRole(element);
        };

        const collectTextMatches = (rootNode, target, exact) => {
          const root = rootNode || document;
          const walkerRoot = root instanceof Document ? (root.body || root.documentElement || root) : root;
          const matches = [];
          const walker = document.createTreeWalker(walkerRoot, NodeFilter.SHOW_ELEMENT);
          while (walker.nextNode()) {
            const element = walker.currentNode;
            const text = getInnerText(element);
            if (!text) continue;
            if ((exact && text === target) || (!exact && text.includes(target))) {
              matches.push(element);
            }
          }
          return matches;
        };

        const resolveLocatorElements = (locator, rootNode = document) => {
          const root = rootNode || document;
          switch (locator.by) {
            case "css":
              return Array.from(root.querySelectorAll(locator.value));
            case "testid":
              return Array.from(root.querySelectorAll(`[data-testid="${CSS.escape(locator.value)}"], [data-test-id="${CSS.escape(locator.value)}"]`));
            case "text":
              return collectTextMatches(root, String(locator.value), locator.exact === true);
            case "role": {
              const role = String(locator.value).trim().toLowerCase();
              const exact = locator.exact === true;
              const nameFilter = typeof locator.name === "string" ? locator.name.trim() : "";
              const scope = root instanceof Document ? (root.body || root.documentElement || root) : root;
              const elements = [];
              const rootElement = scope instanceof Element ? scope : null;
              if (rootElement && getElementRole(rootElement) === role) {
                elements.push(rootElement);
              }
              const walker = document.createTreeWalker(scope, NodeFilter.SHOW_ELEMENT);
              while (walker.nextNode()) {
                const element = walker.currentNode;
                if (!(element instanceof Element)) continue;
                if (getElementRole(element) !== role) continue;
                if (nameFilter) {
                  const accessibleName = getElementAccessibleName(element);
                  if (!accessibleName) continue;
                  const matchesName = exact ? accessibleName === nameFilter : accessibleName.includes(nameFilter);
                  if (!matchesName) continue;
                }
                elements.push(element);
              }
              return elements;
            }
            case "label": {
              const target = String(locator.value);
              const exact = locator.exact === true;
              const scope = root instanceof Document ? (root.body || root.documentElement || root) : root;
              const controls = Array.from(
                scope.querySelectorAll("input, textarea, select, button, output, meter, progress")
              );
              return controls.filter((element) => {
                const labelText = getElementLabelText(element);
                if (!labelText) return false;
                return exact ? labelText === target : labelText.includes(target);
              });
            }
            case "placeholder": {
              const target = String(locator.value);
              const exact = locator.exact === true;
              const scope = root instanceof Document ? (root.body || root.documentElement || root) : root;
              const controls = Array.from(
                scope.querySelectorAll("input[placeholder], textarea[placeholder]")
              );
              return controls.filter((element) => {
                const placeholder = String(element.getAttribute("placeholder") || "").trim();
                if (!placeholder) return false;
                return exact ? placeholder === target : placeholder.includes(target);
              });
            }
            case "xpath": {
              const results = [];
              const iterator = document.evaluate(locator.value, root, null, XPathResult.ORDERED_NODE_ITERATOR_TYPE, null);
              let current = iterator.iterateNext();
              while (current) {
                if (current instanceof Element) {
                  results.push(current);
                }
                current = iterator.iterateNext();
              }
              return results;
            }
            default:
              throw new Error(`Unsupported locator strategy: ${locator.by}`);
          }
        };

        const resolveLocatorSnapshot = (locator, rootNode = document) => {
          const matches = resolveLocatorElements(locator, rootNode);
          const index = Number.isInteger(locator.nth) && locator.nth >= 0 ? locator.nth : 0;
          const element = matches[index] || null;
          const ambiguous = !Number.isInteger(locator.nth) && matches.length > 1;
          return {
            locator,
            strategy: locator.by,
            index,
            count: matches.length,
            exists: element !== null,
            visible: isVisible(element),
            text: getInnerText(element),
            matchedTexts: matches
              .map((candidate) => getInnerText(candidate))
              .filter(Boolean)
              .slice(0, 20),
            ambiguous,
            ambiguityMessage: ambiguous
              ? `Locator matched ${matches.length} elements and no nth was provided. Pass locator.nth to disambiguate.`
              : null,
          };
        };

        const summarizeElement = (element, index) => {
          if (!element) return null;
          const rect = element.getBoundingClientRect();
          return {
            index,
            tagName: String(element.tagName || "").toLowerCase(),
            id: element.id || null,
            className: typeof element.className === "string" ? element.className : null,
            text: getInnerText(element).slice(0, 500),
            visible: isVisible(element),
            rect: {
              x: rect.x,
              y: rect.y,
              width: rect.width,
              height: rect.height,
            },
            attributes: {
              name: element.getAttribute("name"),
              type: element.getAttribute("type"),
              role: element.getAttribute("role"),
              href: element.getAttribute("href"),
              value: element.getAttribute("value"),
              testid: element.getAttribute("data-testid") || element.getAttribute("data-test-id"),
            },
          };
        };
    "#
  }

  fn locator_input_schema(description: &str) -> serde_json::Value {
    serde_json::json!({
      "type": "object",
      "description": description,
      "properties": {
        "by": {
          "type": "string",
          "enum": McpLocatorBy::schema_values(),
          "description": "Locator strategy"
        },
        "value": {
          "type": "string",
          "description": "Locator value. Cannot be empty."
        },
        "nth": {
          "type": "integer",
          "description": "Zero-based match index. Default is 0."
        },
        "exact": {
          "type": "boolean",
          "description": "For text/role-name/label/placeholder locators, require exact text equality instead of contains"
        },
        "name": {
          "type": "string",
          "description": "Optional accessible name filter for role locators. For by=role, combines with exact for exact-vs-contains matching."
        }
      },
      "required": ["by", "value"]
    })
  }

  #[allow(dead_code)]
  fn frame_ref_input_schema(description: &str) -> serde_json::Value {
    serde_json::json!({
      "type": "object",
      "description": description,
      "properties": {
        "by": {
          "type": "string",
          "enum": McpFrameBy::schema_values(),
          "description": "Frame selection strategy: main, index, url, name, or id"
        },
        "value": {
          "description": "Selector value. For by=index this must be an integer. For by=url/name/id this must be a non-empty string. Do not provide value when by=main.",
          "oneOf": [
            { "type": "integer" },
            { "type": "string" }
          ]
        }
      },
      "required": ["by"]
    })
  }

  fn tab_ref_input_schema(description: &str) -> serde_json::Value {
    serde_json::json!({
      "type": "object",
      "description": description,
      "properties": {
        "by": {
          "type": "string",
          "enum": McpTabBy::schema_values(),
          "description": "Tab selection strategy: id, index, url, or title"
        },
        "value": {
          "description": "Selector value. For by=index this must be an integer. For by=id/url/title this must be a non-empty string.",
          "oneOf": [
            { "type": "integer" },
            { "type": "string" }
          ]
        }
      },
      "required": ["by", "value"]
    })
  }

  fn normalize_optional_text(value: Option<&serde_json::Value>) -> Option<String> {
    value
      .and_then(|value| value.as_str())
      .map(str::trim)
      .filter(|value| !value.is_empty())
      .map(ToString::to_string)
  }

  fn parse_cdp_frame_tree(value: &serde_json::Value) -> Result<McpFrameTreeNode, McpError> {
    let frame_tree = value
      .get("frameTree")
      .ok_or_else(|| McpError::internal("CDP response missing frameTree"))?;
    let mut next_index = 0usize;
    Self::parse_cdp_frame_tree_node(frame_tree, 0, &mut next_index)
  }

  fn parse_cdp_frame_tree_node(
    node: &serde_json::Value,
    depth: usize,
    next_index: &mut usize,
  ) -> Result<McpFrameTreeNode, McpError> {
    let frame = node
      .get("frame")
      .ok_or_else(|| McpError::internal("CDP frameTree node missing frame payload"))?;
    let id = frame
      .get("id")
      .and_then(|value| value.as_str())
      .filter(|value| !value.trim().is_empty())
      .ok_or_else(|| McpError::internal("CDP frame payload missing frame id"))?
      .to_string();
    let parent_id = Self::normalize_optional_text(frame.get("parentId"));
    let index = *next_index;
    *next_index += 1;

    let mut children = Vec::new();
    if let Some(child_frames) = node.get("childFrames").and_then(|value| value.as_array()) {
      for child in child_frames {
        children.push(Self::parse_cdp_frame_tree_node(
          child,
          depth + 1,
          next_index,
        )?);
      }
    }

    Ok(McpFrameTreeNode {
      id,
      parent_id: parent_id.clone(),
      index,
      depth,
      url: frame
        .get("url")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string(),
      name: Self::normalize_optional_text(frame.get("name")),
      origin: Self::normalize_optional_text(frame.get("securityOrigin")),
      main: parent_id.is_none(),
      children,
    })
  }

  fn flatten_frame_tree(root: &McpFrameTreeNode) -> Vec<McpFrameNode> {
    let mut frames = Vec::new();
    Self::collect_frame_nodes(root, &mut frames);
    frames
  }

  fn collect_frame_nodes(node: &McpFrameTreeNode, output: &mut Vec<McpFrameNode>) {
    output.push(McpFrameNode {
      id: node.id.clone(),
      parent_id: node.parent_id.clone(),
      index: node.index,
      depth: node.depth,
      url: node.url.clone(),
      name: node.name.clone(),
      origin: node.origin.clone(),
      main: node.main,
      child_count: node.children.len(),
    });

    for child in &node.children {
      Self::collect_frame_nodes(child, output);
    }
  }

  fn frame_matches_selector(frame: &McpFrameNode, selector: &McpFrameRef) -> bool {
    match (&selector.by, &selector.value) {
      (McpFrameBy::Main, _) => frame.main,
      (McpFrameBy::Index, Some(McpFrameValue::Index(index))) => frame.index == *index,
      (McpFrameBy::Id, Some(McpFrameValue::Text(value))) => frame.id == *value,
      (McpFrameBy::Name, Some(McpFrameValue::Text(value))) => {
        frame.name.as_deref() == Some(value.as_str())
      }
      (McpFrameBy::Url, Some(McpFrameValue::Text(value))) => frame.url.contains(value),
      _ => false,
    }
  }

  fn resolve_frame_selector(
    frames: &[McpFrameNode],
    selector: &McpFrameRef,
  ) -> Result<McpFrameNode, McpError> {
    selector.validate()?;
    let matches = frames
      .iter()
      .filter(|frame| Self::frame_matches_selector(frame, selector))
      .cloned()
      .collect::<Vec<_>>();

    match matches.as_slice() {
      [single] => Ok(single.clone()),
      [] => Err(McpError::not_found(format!(
        "No frame matched {}",
        selector.describe()
      ))),
      many => {
        let candidates = many
          .iter()
          .take(5)
          .map(|frame| {
            serde_json::json!({
              "id": frame.id,
              "index": frame.index,
              "url": frame.url,
              "name": frame.name,
            })
          })
          .collect::<Vec<_>>();
        Err(McpError::invalid_params(format!(
          "{} matched multiple frames ({}). Refine the selector. Candidates: {}",
          selector.describe(),
          many.len(),
          serde_json::to_string(&candidates).unwrap_or_else(|_| "[]".to_string())
        )))
      }
    }
  }

  fn tab_matches_selector(tab: &McpTabNode, selector: &McpTabRef) -> bool {
    match (&selector.by, &selector.value) {
      (McpTabBy::Id, Some(McpTabValue::Text(value))) => tab.id == *value,
      (McpTabBy::Index, Some(McpTabValue::Index(index))) => tab.index == *index,
      (McpTabBy::Index, Some(McpTabValue::Text(value))) => value
        .trim()
        .parse::<usize>()
        .map(|index| tab.index == index)
        .unwrap_or(false),
      (McpTabBy::Url, Some(McpTabValue::Text(value))) => tab.url.contains(value),
      (McpTabBy::Title, Some(McpTabValue::Text(value))) => tab.title.contains(value),
      _ => false,
    }
  }

  fn resolve_tab_selector(
    tabs: &[McpTabNode],
    selector: &McpTabRef,
  ) -> Result<McpTabNode, McpError> {
    selector.validate()?;
    let matches = tabs
      .iter()
      .filter(|tab| Self::tab_matches_selector(tab, selector))
      .cloned()
      .collect::<Vec<_>>();

    match matches.as_slice() {
      [single] => Ok(single.clone()),
      [] => Err(McpError::not_found(format!(
        "No tab matched {}",
        selector.describe()
      ))),
      many => {
        let candidates = many
          .iter()
          .take(5)
          .map(|tab| {
            serde_json::json!({
              "id": tab.id,
              "index": tab.index,
              "title": tab.title,
              "url": tab.url,
            })
          })
          .collect::<Vec<_>>();
        Err(McpError::invalid_params(format!(
          "{} matched multiple tabs ({}). Refine the selector. Candidates: {}",
          selector.describe(),
          many.len(),
          serde_json::to_string(&candidates).unwrap_or_else(|_| "[]".to_string())
        )))
      }
    }
  }

  fn parse_execution_context_id(result: &serde_json::Value) -> Result<i64, McpError> {
    result
      .get("executionContextId")
      .and_then(|value| value.as_i64().or_else(|| value.as_u64().map(|v| v as i64)))
      .ok_or_else(|| McpError::internal("CDP response missing executionContextId"))
  }

  fn validate_selector(selector: &str) -> Result<(), McpError> {
    if selector.trim().is_empty() {
      return Err(McpError::invalid_params(
        "Selector cannot be empty or whitespace only",
      ));
    }
    Ok(())
  }

  fn validate_element_target(
    selector: Option<&str>,
    locator: Option<&McpLocator>,
    tool_name: &str,
  ) -> Result<(), McpError> {
    match (selector, locator) {
      (Some(selector), None) => Self::validate_selector(selector),
      (None, Some(locator)) => locator.validate(),
      (Some(_), Some(_)) => Err(McpError::invalid_params(format!(
        "Provide either selector or locator for {tool_name}, not both",
      ))),
      (None, None) => Err(McpError::invalid_params(format!(
        "Missing selector or locator for {tool_name}",
      ))),
    }
  }

  fn validate_non_empty_key(key: &str, field_name: &str) -> Result<(), McpError> {
    if key.trim().is_empty() {
      return Err(McpError::invalid_params(format!(
        "{field_name} cannot be empty or whitespace only"
      )));
    }
    Ok(())
  }

  fn validate_select_option_args(args: &SelectOptionArgs) -> Result<(), McpError> {
    Self::validate_element_target(
      args.selector.as_deref(),
      args.locator.as_ref(),
      "select_option",
    )?;

    let mut provided = 0;
    if let Some(value) = args.value.as_deref() {
      Self::validate_non_empty_key(value, "value")?;
      provided += 1;
    }
    if let Some(label) = args.label.as_deref() {
      Self::validate_non_empty_key(label, "label")?;
      provided += 1;
    }
    if args.index.is_some() {
      provided += 1;
    }

    if provided != 1 {
      return Err(McpError::invalid_params(
        "Provide exactly one of value, label, or index for select_option",
      ));
    }

    Ok(())
  }

  fn locator_is_ambiguous_without_nth(locator: &McpLocator, count: usize) -> bool {
    locator.nth.is_none() && count > 1
  }

  fn ensure_single_target_locator(
    locator: &McpLocator,
    snapshot: &LocatorResolutionSnapshot,
    operation: &str,
  ) -> Result<(), McpError> {
    if !Self::locator_is_ambiguous_without_nth(locator, snapshot.count) {
      return Ok(());
    }

    let matched_texts = snapshot
      .matched_texts
      .iter()
      .filter(|text| !text.trim().is_empty())
      .take(5)
      .cloned()
      .collect::<Vec<_>>();
    let matched_texts_json =
      serde_json::to_string(&matched_texts).unwrap_or_else(|_| "[]".to_string());

    Err(McpError::invalid_params(format!(
      "Ambiguous locator for {operation}: matched {} elements and no locator.nth was provided. Pass locator.nth to disambiguate. Matched texts: {}",
      snapshot.count, matched_texts_json
    )))
  }

  fn validate_file_input_args(
    args: &FileInputArgs,
    tool_name: &str,
  ) -> Result<Vec<std::path::PathBuf>, McpError> {
    Self::validate_element_target(args.selector.as_deref(), args.locator.as_ref(), tool_name)?;

    if let Some(frame) = args.frame.as_ref() {
      frame.validate()?;
    }

    if args.files.is_empty() {
      return Err(McpError::invalid_params(format!(
        "files cannot be empty for {tool_name}"
      )));
    }

    let mut resolved_paths = Vec::with_capacity(args.files.len());
    for file in &args.files {
      Self::validate_non_empty_key(file, "files")?;
      let path = std::path::PathBuf::from(file);
      let metadata = std::fs::metadata(&path).map_err(|e| {
        McpError::invalid_params(format!(
          "File does not exist or is not accessible: {file} ({e})"
        ))
      })?;
      if !metadata.is_file() {
        return Err(McpError::invalid_params(format!(
          "File path must point to a regular file: {file}"
        )));
      }
      let canonical = std::fs::canonicalize(&path).map_err(|e| {
        McpError::invalid_params(format!("Failed to canonicalize file path {file}: {e}"))
      })?;
      resolved_paths.push(canonical);
    }

    Ok(resolved_paths)
  }

  fn build_file_input_readiness_expression(
    selector: Option<&str>,
    locator: Option<&McpLocator>,
  ) -> Result<String, McpError> {
    Self::build_element_target_expression(
      selector,
      locator,
      r#"
          const rect = element instanceof Element ? element.getBoundingClientRect() : { width: 0, height: 0 };
          const visible = rect.width > 0 && rect.height > 0;
          const disabled = !!element.disabled;
          const isInput = element instanceof HTMLInputElement;
          const inputType = isInput ? String(element.getAttribute("type") || "").toLowerCase() : null;
          const ok = isInput && inputType === "file" && visible && !disabled;

          return {
            ok,
            visible,
            disabled,
            isInput,
            inputType,
            multiple: isInput ? !!element.multiple : false,
            target,
            error: !isInput ? "Target element is not an <input>" :
              inputType !== "file" ? "Target element is not a file input" :
              !visible ? "File input is not visible" :
              disabled ? "File input is disabled" :
              null,
          };
      "#,
    )
  }

  fn build_element_target_expression(
    selector: Option<&str>,
    locator: Option<&McpLocator>,
    action_body: &str,
  ) -> Result<String, McpError> {
    Self::validate_element_target(selector, locator, "element action")?;

    let selector_literal = serde_json::to_string(&selector)
      .map_err(|e| McpError::internal(format!("Failed to serialize selector: {e}")))?;
    let locator_literal = serde_json::to_string(&locator)
      .map_err(|e| McpError::internal(format!("Failed to serialize locator: {e}")))?;

    Ok(format!(
      r#"(() => {{
        const selector = {selector_literal};
        const locator = {locator_literal};
        {locator_helpers}

        const resolveTarget = () => {{
          if (selector) {{
            return {{
              element: document.querySelector(selector),
              target: {{
                type: "selector",
                selector,
              }},
            }};
          }}

          const resolution = resolveLocatorSnapshot(locator);
          if (resolution && resolution.error) {{
            throw new Error(resolution.error);
          }}
          if (resolution.ambiguous) {{
            throw new Error(
              `${{resolution.ambiguityMessage}} Matched texts: ${{JSON.stringify((resolution.matchedTexts || []).slice(0, 5))}}`
            );
          }}
          const matches = resolveLocatorElements(locator);
          return {{
            element: matches[resolution.index] || null,
            target: {{
              type: "locator",
              locator,
              resolution,
            }},
          }};
        }};

        const describeTarget = (target) => {{
          if (target?.type === "selector") {{
            return `selector ${{target.selector}}`;
          }}
          return `locator ${{JSON.stringify(target?.locator ?? null)}}`;
        }};

        try {{
          const {{ element, target }} = resolveTarget();
          if (!element) {{
            return {{
              ok: false,
              error: `Element not found for ${{describeTarget(target)}}`,
              target,
            }};
          }}

          {action_body}
        }} catch (error) {{
          return {{
            ok: false,
            error: error instanceof Error ? error.message : String(error),
            selector,
            locator,
          }};
        }}
      }})()"#,
      locator_helpers = Self::locator_helpers_script()
    ))
  }

  fn build_element_actionability_expression(
    selector: Option<&str>,
    locator: Option<&McpLocator>,
    mode: &str,
  ) -> Result<String, McpError> {
    if !matches!(mode, "click" | "type") {
      return Err(McpError::invalid_params(format!(
        "Unsupported auto-wait mode: {mode}"
      )));
    }

    Self::validate_element_target(selector, locator, "element auto-wait")?;
    let selector_literal = serde_json::to_string(&selector)
      .map_err(|e| McpError::internal(format!("Failed to serialize selector: {e}")))?;
    let locator_literal = serde_json::to_string(&locator)
      .map_err(|e| McpError::internal(format!("Failed to serialize locator: {e}")))?;
    let mode_literal = serde_json::to_string(mode)
      .map_err(|e| McpError::internal(format!("Failed to serialize auto-wait mode: {e}")))?;

    Ok(format!(
      r#"(() => {{
        const selector = {selector_literal};
        const locator = {locator_literal};
        const mode = {mode_literal};
        {locator_helpers}

        const resolveTarget = () => {{
          if (selector) {{
            return {{
              element: document.querySelector(selector),
              target: {{
                type: "selector",
                selector,
              }},
            }};
          }}

          const resolution = resolveLocatorSnapshot(locator);
          if (resolution && resolution.error) {{
            throw new Error(resolution.error);
          }}
          if (resolution.ambiguous) {{
            throw new Error(
              `${{resolution.ambiguityMessage}} Matched texts: ${{JSON.stringify((resolution.matchedTexts || []).slice(0, 5))}}`
            );
          }}
          const matches = resolveLocatorElements(locator);
          return {{
            element: matches[resolution.index] || null,
            target: {{
              type: "locator",
              locator,
              resolution,
            }},
          }};
        }};

        try {{
          const {{ element, target }} = resolveTarget();
          if (!element) {{
            return {{
              ok: false,
              ready: false,
              target,
              exists: false,
              visible: false,
              hasSize: false,
              enabled: false,
              editable: false,
              focused: false,
              reason: "element_not_found",
            }};
          }}

          element.scrollIntoView({{ block: "center", inline: "center" }});
          const rect = element.getBoundingClientRect();
          const hasSize = rect.width > 0 && rect.height > 0;
          const visible = isVisible(element);
          const disabled = !!(
            ("disabled" in element && element.disabled === true) ||
            element.getAttribute("disabled") !== null ||
            element.getAttribute("aria-disabled") === "true"
          );
          const readOnly = !!(
            ("readOnly" in element && element.readOnly === true) ||
            element.getAttribute("readonly") !== null
          );
          const editable = !!(
            !disabled &&
            !readOnly &&
            (
              element.isContentEditable ||
              element instanceof HTMLInputElement ||
              element instanceof HTMLTextAreaElement
            )
          );

          let focused = document.activeElement === element;
          if (mode === "type" && typeof element.focus === "function") {{
            try {{
              element.focus();
            }} catch (_error) {{}}
            focused = document.activeElement === element;
          }}

          const enabled = !disabled;
          const ready = mode === "click"
            ? visible && hasSize && enabled
            : visible && hasSize && enabled && editable && focused;

          return {{
            ok: ready,
            ready,
            mode,
            target,
            exists: true,
            visible,
            hasSize,
            enabled,
            editable,
            focused,
            readOnly,
            reason: ready ? "ready" : (
              !visible ? "element_not_visible" :
              !hasSize ? "element_has_no_size" :
              !enabled ? "element_disabled" :
              mode === "type" && !editable ? "element_not_editable" :
              mode === "type" && !focused ? "focus_failed" :
              "not_ready"
            ),
            rect: {{
              x: rect.x,
              y: rect.y,
              width: rect.width,
              height: rect.height,
            }},
            text: getInnerText(element),
            tagName: element.tagName,
          }};
        }} catch (error) {{
          return {{
            ok: false,
            ready: false,
            selector,
            locator,
            error: error instanceof Error ? error.message : String(error),
          }};
        }}
      }})()"#,
      locator_helpers = Self::locator_helpers_script()
    ))
  }

  async fn wait_for_element_actionable(
    &self,
    ws_url: &str,
    selector: Option<&str>,
    locator: Option<&McpLocator>,
    context_id: Option<i64>,
    mode: &str,
    timeout_ms: u64,
  ) -> Result<serde_json::Value, McpError> {
    let expression = Self::build_element_actionability_expression(selector, locator, mode)?;
    let last_observed = Arc::new(Mutex::new(None::<serde_json::Value>));

    self
      .wait_with_timeout(
        timeout_ms,
        || {
          let last_observed = last_observed.clone();
          let expression = expression.clone();
          let ws_url = ws_url.to_string();
          async move {
            let value = self
              .evaluate_runtime_value(&ws_url, &expression, context_id)
              .await?;
            *last_observed.lock().expect("actionability lock poisoned") = Some(value.clone());

            if let Some(error) = value.get("error").and_then(|value| value.as_str()) {
              return Err(McpError::invalid_params(error));
            }

            if value.get("ready").and_then(|value| value.as_bool()) == Some(true) {
              Ok(Some(value))
            } else {
              Ok(None)
            }
          }
        },
        || {
          let last_observed = last_observed
            .lock()
            .expect("actionability lock poisoned")
            .clone()
            .unwrap_or(serde_json::Value::Null);
          format!(
            "Timed out after {timeout_ms}ms waiting for {mode} target to become actionable. Last observed state: {}",
            serde_json::to_string(&last_observed).unwrap_or_else(|_| "null".to_string())
          )
        },
      )
      .await
  }

  async fn wait_for_locator_snapshot(
    &self,
    ws_url: &str,
    locator: &McpLocator,
    context_id: Option<i64>,
    timeout_ms: u64,
  ) -> Result<(LocatorResolutionSnapshot, bool), McpError> {
    let expression = Self::build_locator_resolution_expression(locator)?;
    let last_observed = Arc::new(Mutex::new(None::<serde_json::Value>));

    match self
      .wait_with_timeout(
        timeout_ms,
        || {
          let last_observed = last_observed.clone();
          let expression = expression.clone();
          let ws_url = ws_url.to_string();
          async move {
            let value = self
              .evaluate_runtime_value(&ws_url, &expression, context_id)
              .await?;
            *last_observed
              .lock()
              .expect("locator snapshot lock poisoned") = Some(value.clone());

            if let Some(error) = value.get("error").and_then(|value| value.as_str()) {
              return Err(McpError::invalid_params(error));
            }

            let snapshot = Self::parse_locator_resolution_snapshot(value)?;
            if snapshot.exists {
              Ok(Some(snapshot))
            } else {
              Ok(None)
            }
          }
        },
        || {
          let last_observed = last_observed
            .lock()
            .expect("locator snapshot lock poisoned")
            .clone()
            .unwrap_or(serde_json::Value::Null);
          format!(
            "Timed out after {timeout_ms}ms waiting for locator {:?}. Last observed state: {}",
            locator,
            serde_json::to_string(&last_observed).unwrap_or_else(|_| "null".to_string())
          )
        },
      )
      .await
    {
      Ok(snapshot) => Ok((snapshot, false)),
      Err(error) if error.code == MCP_ERROR_TIMEOUT => {
        let fallback = last_observed
          .lock()
          .expect("locator snapshot lock poisoned")
          .clone()
          .unwrap_or_else(|| {
            serde_json::json!({
              "locator": locator,
              "strategy": locator.by.as_str(),
              "index": locator.nth.unwrap_or(0),
              "count": 0,
              "exists": false,
              "visible": false,
              "text": "",
              "matchedTexts": [],
            })
          });
        let snapshot = Self::parse_locator_resolution_snapshot(fallback)?;
        Ok((snapshot, true))
      }
      Err(error) => Err(error),
    }
  }

  async fn wait_for_locator_collection(
    &self,
    ws_url: &str,
    locator: &McpLocator,
    limit: usize,
    context_id: Option<i64>,
    timeout_ms: u64,
  ) -> Result<(serde_json::Value, bool), McpError> {
    let expression = Self::build_locator_collection_expression(locator, limit)?;
    let last_observed = Arc::new(Mutex::new(None::<serde_json::Value>));

    match self
      .wait_with_timeout(
        timeout_ms,
        || {
          let last_observed = last_observed.clone();
          let expression = expression.clone();
          let ws_url = ws_url.to_string();
          async move {
            let value = self
              .evaluate_runtime_value(&ws_url, &expression, context_id)
              .await?;
            *last_observed
              .lock()
              .expect("locator collection lock poisoned") = Some(value.clone());

            if let Some(error) = value.get("error").and_then(|value| value.as_str()) {
              return Err(McpError::invalid_params(error));
            }

            let count = value.get("count").and_then(|value| value.as_u64()).unwrap_or(0);
            if count > 0 { Ok(Some(value)) } else { Ok(None) }
          }
        },
        || {
          let last_observed = last_observed
            .lock()
            .expect("locator collection lock poisoned")
            .clone()
            .unwrap_or(serde_json::Value::Null);
          format!(
            "Timed out after {timeout_ms}ms waiting for locator {:?} to return any elements. Last observed state: {}",
            locator,
            serde_json::to_string(&last_observed).unwrap_or_else(|_| "null".to_string())
          )
        },
      )
      .await
    {
      Ok(value) => Ok((value, false)),
      Err(error) if error.code == MCP_ERROR_TIMEOUT => {
        let fallback = last_observed
          .lock()
          .expect("locator collection lock poisoned")
          .clone()
          .unwrap_or_else(|| {
            serde_json::json!({
              "locator": locator,
              "count": 0,
              "elements": [],
            })
          });
        Ok((fallback, true))
      }
      Err(error) => Err(error),
    }
  }

  fn build_locator_resolution_expression(locator: &McpLocator) -> Result<String, McpError> {
    locator.validate()?;
    let locator_literal = serde_json::to_string(locator)
      .map_err(|e| McpError::internal(format!("Failed to serialize locator: {e}")))?;

    Ok(format!(
      r#"(() => {{
        const locator = {locator_literal};
        {locator_helpers}

        try {{
          return resolveLocatorSnapshot(locator);
        }} catch (error) {{
          return {{
            error: error instanceof Error ? error.message : String(error),
            locator,
          }};
        }}
      }})()"#,
      locator_helpers = Self::locator_helpers_script()
    ))
  }

  fn build_locator_collection_expression(
    locator: &McpLocator,
    limit: usize,
  ) -> Result<String, McpError> {
    locator.validate()?;
    let locator_literal = serde_json::to_string(locator)
      .map_err(|e| McpError::internal(format!("Failed to serialize locator: {e}")))?;

    Ok(format!(
      r#"(() => {{
        const locator = {locator_literal};
        const limit = {limit};
        {locator_helpers}

        try {{
          const matches = resolveLocatorElements(locator);
          return {{
            locator,
            count: matches.length,
            elements: matches
              .slice(0, limit)
              .map((element, index) => summarizeElement(element, index))
              .filter(Boolean),
          }};
        }} catch (error) {{
          return {{
            error: error instanceof Error ? error.message : String(error),
            locator,
          }};
        }}
      }})()"#,
      locator_helpers = Self::locator_helpers_script()
    ))
  }

  #[allow(dead_code)]
  async fn evaluate_locator_snapshot(
    &self,
    profile_id: &str,
    locator: &McpLocator,
  ) -> Result<LocatorResolutionSnapshot, McpError> {
    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    self
      .evaluate_locator_snapshot_in_context(&ws_url, locator, None)
      .await
  }

  async fn evaluate_locator_snapshot_in_context(
    &self,
    ws_url: &str,
    locator: &McpLocator,
    context_id: Option<i64>,
  ) -> Result<LocatorResolutionSnapshot, McpError> {
    let expression = Self::build_locator_resolution_expression(locator)?;
    let value = self
      .evaluate_runtime_value(ws_url, &expression, context_id)
      .await?;

    if let Some(error) = value.get("error").and_then(|value| value.as_str()) {
      return Err(McpError::invalid_params(error));
    }

    Self::parse_locator_resolution_snapshot(value)
  }

  #[allow(dead_code)]
  async fn evaluate_locator_collection(
    &self,
    profile_id: &str,
    locator: &McpLocator,
    limit: usize,
  ) -> Result<serde_json::Value, McpError> {
    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    self
      .evaluate_locator_collection_in_context(&ws_url, locator, limit, None)
      .await
  }

  #[allow(dead_code)]
  async fn evaluate_locator_collection_in_context(
    &self,
    ws_url: &str,
    locator: &McpLocator,
    limit: usize,
    context_id: Option<i64>,
  ) -> Result<serde_json::Value, McpError> {
    let expression = Self::build_locator_collection_expression(locator, limit)?;
    let value = self
      .evaluate_runtime_value(ws_url, &expression, context_id)
      .await?;

    if let Some(error) = value.get("error").and_then(|value| value.as_str()) {
      return Err(McpError::invalid_params(error));
    }

    Ok(value)
  }

  fn parse_locator_resolution_snapshot(
    value: serde_json::Value,
  ) -> Result<LocatorResolutionSnapshot, McpError> {
    serde_json::from_value(value)
      .map_err(|e| McpError::internal(format!("Failed to parse locator resolution snapshot: {e}")))
  }

  fn build_locator_condition_expression(
    locator: &McpLocator,
    state: &str,
  ) -> Result<String, McpError> {
    let state_literal = serde_json::to_string(state)
      .map_err(|e| McpError::internal(format!("Failed to serialize wait state: {e}")))?;
    let resolution_expression = Self::build_locator_resolution_expression(locator)?;

    Ok(format!(
      r#"(() => {{
        const state = {state_literal};
        const resolved = {resolution_expression};

        if (resolved && resolved.error) {{
          return resolved;
        }}

        return {{
          ...resolved,
          ok: state === "exists" ? resolved.exists : resolved.visible,
          state,
        }};
      }})()"#
    ))
  }

  fn build_wait_for_text_expression(
    text: &str,
    selector: Option<&str>,
    locator: Option<&McpLocator>,
    exact: bool,
  ) -> Result<String, McpError> {
    if text.trim().is_empty() {
      return Err(McpError::invalid_params("Text to wait for cannot be empty"));
    }
    if matches!(selector, Some(value) if value.trim().is_empty()) {
      return Err(McpError::invalid_params("Selector cannot be empty"));
    }
    if selector.is_some() && locator.is_some() {
      return Err(McpError::invalid_params(
        "Provide either selector or locator for wait_for_text, not both",
      ));
    }

    if let Some(locator) = locator {
      locator.validate()?;
    }

    let text_literal = serde_json::to_string(text)
      .map_err(|e| McpError::internal(format!("Failed to serialize wait text: {e}")))?;
    let selector_literal = serde_json::to_string(&selector)
      .map_err(|e| McpError::internal(format!("Failed to serialize selector: {e}")))?;
    let locator_literal = serde_json::to_string(&locator)
      .map_err(|e| McpError::internal(format!("Failed to serialize locator: {e}")))?;
    let exact_literal = serde_json::to_string(&exact)
      .map_err(|e| McpError::internal(format!("Failed to serialize exact flag: {e}")))?;

    Ok(format!(
      r#"(() => {{
        const targetText = {text_literal};
        const selector = {selector_literal};
        const locator = {locator_literal};
        const exact = {exact_literal};
        {locator_helpers}

        try {{
          let scopeType = "page";
          let scopeElements = [];
          let locatorResolution = null;

          if (selector) {{
            scopeType = "selector";
            scopeElements = Array.from(document.querySelectorAll(selector));
          }} else if (locator) {{
            scopeType = "locator";
            locatorResolution = resolveLocatorSnapshot(locator);
            scopeElements = locatorResolution.exists
              ? [resolveLocatorElements(locator)[locatorResolution.index]].filter(Boolean)
              : [];
          }}

          const scopedTexts = scopeType === "page"
            ? [getInnerText(document.body || document.documentElement)]
            : scopeElements.map((element) => getInnerText(element)).filter(Boolean);
          const observedText = scopedTexts.join("\n").trim();
          const matchCount = scopedTexts.filter((value) => exact ? value === targetText : value.includes(targetText)).length;
          const ok = exact ? observedText === targetText || matchCount > 0 : observedText.includes(targetText) || matchCount > 0;

          return {{
            ok,
            scopeType,
            selector,
            locator,
            locatorResolution,
            exact,
            targetText,
            scopeCount: scopeElements.length,
            matchCount,
            observedTextPreview: observedText.slice(0, 500),
          }};
        }} catch (error) {{
          return {{
            error: error instanceof Error ? error.message : String(error),
            selector,
            locator,
            targetText,
          }};
        }}
      }})()"#,
      locator_helpers = Self::locator_helpers_script()
    ))
  }

  fn build_navigation_snapshot_expression() -> &'static str {
    r#"(() => ({
      url: window.location.href,
      title: document.title,
      readyState: document.readyState,
    }))()"#
  }

  fn parse_navigation_snapshot(value: serde_json::Value) -> Result<NavigationSnapshot, McpError> {
    serde_json::from_value(value).map_err(|e| {
      McpError::internal(format!(
        "Failed to parse navigation snapshot from page context: {e}"
      ))
    })
  }

  fn navigation_wait_satisfied(
    baseline_url: &str,
    current: &NavigationSnapshot,
    saw_non_complete: bool,
  ) -> bool {
    current.ready_state == "complete" && (current.url != baseline_url || saw_non_complete)
  }

  async fn run_console_capture(
    profile_id: String,
    ws_url: String,
    shared: ConsoleCaptureShared,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ready_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
    timeout_ms: u64,
  ) {
    use futures_util::sink::SinkExt;
    use futures_util::stream::StreamExt;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    let timeout = std::time::Duration::from_millis(timeout_ms.max(1_000));
    let connect_result = tokio::time::timeout(timeout, connect_async(&ws_url)).await;
    let mut ready_tx = Some(ready_tx);
    let (mut ws_stream, _) = match connect_result {
      Ok(Ok(result)) => result,
      Ok(Err(e)) => {
        let message = format!("Failed to connect console capture for profile {profile_id}: {e}");
        Self::set_console_capture_error(&shared, message.clone()).await;
        Self::notify_capture_ready(&mut ready_tx, Err(message));
        return;
      }
      Err(_) => {
        let message = format!(
          "Timed out connecting console capture for profile {profile_id} after {timeout_ms}ms"
        );
        Self::set_console_capture_error(&shared, message.clone()).await;
        Self::notify_capture_ready(&mut ready_tx, Err(message));
        return;
      }
    };

    for (id, method) in [
      (1_u64, "Runtime.enable"),
      (2_u64, "Log.enable"),
      (3_u64, "Console.enable"),
    ] {
      let command = serde_json::json!({
        "id": id,
        "method": method,
        "params": {}
      });
      if let Err(e) = ws_stream
        .send(Message::Text(command.to_string().into()))
        .await
      {
        let message = format!("Failed to initialize console capture for profile {profile_id}: {e}");
        Self::set_console_capture_error(&shared, message.clone()).await;
        Self::notify_capture_ready(&mut ready_tx, Err(message));
        return;
      }
    }

    let mut pending_responses = std::collections::HashSet::from([1_u64, 2_u64, 3_u64]);
    let mut shutdown_rx = shutdown_rx;

    while !pending_responses.is_empty() {
      let next_message = tokio::select! {
        _ = &mut shutdown_rx => {
          shared.running.store(false, Ordering::SeqCst);
          Self::notify_capture_ready(
            &mut ready_tx,
            Err(format!("Console capture stopped before initialization for profile {profile_id}")),
          );
          return;
        }
        message = tokio::time::timeout(timeout, ws_stream.next()) => message,
      };

      let Some(message) = (match next_message {
        Ok(message) => message,
        Err(_) => {
          let message = format!("Timed out enabling console capture for profile {profile_id}");
          Self::set_console_capture_error(&shared, message.clone()).await;
          Self::notify_capture_ready(&mut ready_tx, Err(message));
          return;
        }
      }) else {
        let message =
          format!("Console capture stream closed while initializing profile {profile_id}");
        Self::set_console_capture_error(&shared, message.clone()).await;
        Self::notify_capture_ready(&mut ready_tx, Err(message));
        return;
      };

      let message = match message {
        Ok(message) => message,
        Err(e) => {
          let message = format!("Console capture stream error for profile {profile_id}: {e}");
          Self::set_console_capture_error(&shared, message.clone()).await;
          Self::notify_capture_ready(&mut ready_tx, Err(message));
          return;
        }
      };

      if let Message::Text(text) = message {
        let response: serde_json::Value = match serde_json::from_str(text.as_str()) {
          Ok(response) => response,
          Err(_) => continue,
        };

        if let Some(id) = response.get("id").and_then(|v| v.as_u64()) {
          if let Some(error) = response.get("error") {
            let message =
              format!("CDP error enabling console capture for profile {profile_id}: {error}");
            Self::set_console_capture_error(&shared, message.clone()).await;
            Self::notify_capture_ready(&mut ready_tx, Err(message));
            return;
          }
          pending_responses.remove(&id);
        }
      }
    }
    Self::notify_capture_ready(&mut ready_tx, Ok(()));

    loop {
      let next_message = tokio::select! {
        _ = &mut shutdown_rx => {
          break;
        }
        message = ws_stream.next() => message,
      };

      let Some(message) = next_message else {
        break;
      };

      let message = match message {
        Ok(message) => message,
        Err(e) => {
          Self::set_console_capture_error(
            &shared,
            format!("Console capture stream error for profile {profile_id}: {e}"),
          )
          .await;
          return;
        }
      };

      let Message::Text(text) = message else {
        continue;
      };

      let payload: serde_json::Value = match serde_json::from_str(text.as_str()) {
        Ok(payload) => payload,
        Err(_) => continue,
      };

      match payload.get("method").and_then(|v| v.as_str()) {
        Some("Runtime.consoleAPICalled") => {
          let params = payload.get("params").cloned().unwrap_or_default();
          let args = params
            .get("args")
            .and_then(|value| value.as_array())
            .map(|items| {
              items
                .iter()
                .map(Self::remote_object_to_text)
                .collect::<Vec<_>>()
                .join(" ")
            })
            .unwrap_or_default();
          let line_number = params
            .get("stackTrace")
            .and_then(|stack| stack.get("callFrames"))
            .and_then(|frames| frames.as_array())
            .and_then(|frames| frames.first())
            .and_then(|frame| frame.get("lineNumber"))
            .and_then(|value| value.as_i64());
          let url = params
            .get("stackTrace")
            .and_then(|stack| stack.get("callFrames"))
            .and_then(|frames| frames.as_array())
            .and_then(|frames| frames.first())
            .and_then(|frame| frame.get("url"))
            .and_then(|value| value.as_str())
            .map(ToString::to_string);

          let entry = ConsoleLogEntry {
            timestamp: params.get("timestamp").and_then(|value| value.as_f64()),
            source: "console".to_string(),
            level: Self::console_level_from_runtime(
              params
                .get("type")
                .and_then(|value| value.as_str())
                .unwrap_or("log"),
            ),
            text: args,
            url,
            line_number,
          };
          Self::push_console_log(&shared, entry).await;
        }
        Some("Log.entryAdded") => {
          let entry_value = payload
            .get("params")
            .and_then(|params| params.get("entry"))
            .cloned()
            .unwrap_or_default();
          let entry = ConsoleLogEntry {
            timestamp: entry_value
              .get("timestamp")
              .and_then(|value| value.as_f64()),
            source: entry_value
              .get("source")
              .and_then(|value| value.as_str())
              .unwrap_or("log")
              .to_string(),
            level: entry_value
              .get("level")
              .and_then(|value| value.as_str())
              .unwrap_or("info")
              .to_string(),
            text: entry_value
              .get("text")
              .and_then(|value| value.as_str())
              .unwrap_or_default()
              .to_string(),
            url: entry_value
              .get("url")
              .and_then(|value| value.as_str())
              .map(ToString::to_string),
            line_number: entry_value
              .get("lineNumber")
              .and_then(|value| value.as_i64()),
          };
          Self::push_console_log(&shared, entry).await;
        }
        Some("Console.messageAdded") => {
          let entry_value = payload
            .get("params")
            .and_then(|params| params.get("message"))
            .cloned()
            .unwrap_or_default();
          let entry = ConsoleLogEntry {
            timestamp: None,
            source: entry_value
              .get("source")
              .and_then(|value| value.as_str())
              .unwrap_or("console")
              .to_string(),
            level: entry_value
              .get("level")
              .and_then(|value| value.as_str())
              .unwrap_or("log")
              .to_string(),
            text: entry_value
              .get("text")
              .and_then(|value| value.as_str())
              .unwrap_or_default()
              .to_string(),
            url: entry_value
              .get("url")
              .and_then(|value| value.as_str())
              .map(ToString::to_string),
            line_number: entry_value
              .get("line")
              .or_else(|| entry_value.get("lineNumber"))
              .and_then(|value| value.as_i64()),
          };
          Self::push_console_log(&shared, entry).await;
        }
        _ => {}
      }
    }

    shared.running.store(false, Ordering::SeqCst);
  }

  async fn handle_network_capture_payload(
    shared: &NetworkCaptureShared,
    payload: serde_json::Value,
  ) {
    match payload.get("method").and_then(|value| value.as_str()) {
      Some("Network.requestWillBeSent") => {
        let params = payload.get("params").cloned().unwrap_or_default();
        let request_id = match params.get("requestId").and_then(|value| value.as_str()) {
          Some(request_id) => request_id.to_string(),
          None => return,
        };
        let request = params.get("request").cloned().unwrap_or_default();
        let resource_type = params
          .get("type")
          .and_then(|value| value.as_str())
          .map(ToString::to_string);
        let initiator_type = params
          .get("initiator")
          .and_then(|value| value.get("type"))
          .and_then(|value| value.as_str())
          .map(ToString::to_string);
        let timestamp = params.get("timestamp").and_then(|value| value.as_f64());

        Self::update_network_request(shared, &request_id, move |entry| {
          entry.url = request
            .get("url")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
          entry.method = request
            .get("method")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
          entry.resource_type = resource_type;
          entry.initiator_type = initiator_type;
          entry.start_timestamp = timestamp;
          entry.request_headers = request
            .get("headers")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
          entry.post_data = request
            .get("postData")
            .and_then(|value| value.as_str())
            .map(ToString::to_string);
          entry.failed = false;
          entry.error_text = None;
          entry.blocked_reason = None;
        })
        .await;
      }
      Some("Network.responseReceived") => {
        let params = payload.get("params").cloned().unwrap_or_default();
        let request_id = match params.get("requestId").and_then(|value| value.as_str()) {
          Some(request_id) => request_id.to_string(),
          None => return,
        };
        let response = params.get("response").cloned().unwrap_or_default();
        let timestamp = params.get("timestamp").and_then(|value| value.as_f64());

        Self::update_network_request(shared, &request_id, move |entry| {
          entry.status = response
            .get("status")
            .and_then(|value| value.as_f64())
            .map(|v| v as i64);
          entry.status_text = response
            .get("statusText")
            .and_then(|value| value.as_str())
            .map(ToString::to_string);
          entry.mime_type = response
            .get("mimeType")
            .and_then(|value| value.as_str())
            .map(ToString::to_string);
          entry.response_headers = response
            .get("headers")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
          if entry.url.is_empty() {
            entry.url = response
              .get("url")
              .and_then(|value| value.as_str())
              .unwrap_or_default()
              .to_string();
          }
          if entry.resource_type.is_none() {
            entry.resource_type = params
              .get("type")
              .and_then(|value| value.as_str())
              .map(ToString::to_string);
          }
          if entry.start_timestamp.is_none() {
            entry.start_timestamp = timestamp;
          }
        })
        .await;
      }
      Some("Network.loadingFinished") => {
        let params = payload.get("params").cloned().unwrap_or_default();
        let request_id = match params.get("requestId").and_then(|value| value.as_str()) {
          Some(request_id) => request_id.to_string(),
          None => return,
        };
        let timestamp = params.get("timestamp").and_then(|value| value.as_f64());
        let encoded_data_length = params
          .get("encodedDataLength")
          .and_then(|value| value.as_f64());

        Self::update_network_request(shared, &request_id, move |entry| {
          entry.end_timestamp = timestamp;
          entry.encoded_data_length = encoded_data_length;
          entry.has_response_body = true;
          if let (Some(start), Some(end)) = (entry.start_timestamp, entry.end_timestamp) {
            entry.duration_ms = Some((end - start) * 1000.0);
          }
        })
        .await;
      }
      Some("Network.loadingFailed") => {
        let params = payload.get("params").cloned().unwrap_or_default();
        let request_id = match params.get("requestId").and_then(|value| value.as_str()) {
          Some(request_id) => request_id.to_string(),
          None => return,
        };
        let timestamp = params.get("timestamp").and_then(|value| value.as_f64());
        let error_text = params
          .get("errorText")
          .and_then(|value| value.as_str())
          .map(ToString::to_string);
        let blocked_reason = params
          .get("blockedReason")
          .and_then(|value| value.as_str())
          .map(ToString::to_string);

        Self::update_network_request(shared, &request_id, move |entry| {
          entry.failed = true;
          entry.error_text = error_text;
          entry.blocked_reason = blocked_reason;
          entry.end_timestamp = timestamp;
          if let (Some(start), Some(end)) = (entry.start_timestamp, entry.end_timestamp) {
            entry.duration_ms = Some((end - start) * 1000.0);
          }
        })
        .await;
      }
      _ => {}
    }
  }

  #[allow(dead_code)]
  async fn run_camoufox_console_capture(
    profile_id: String,
    page: Page,
    shared: ConsoleCaptureShared,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ready_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
  ) {
    let mut ready_tx = Some(ready_tx);
    let mut stream = match page.subscribe_event() {
      Ok(stream) => stream,
      Err(error) => {
        let message =
          format!("Failed to subscribe Camoufox console capture for profile {profile_id}: {error}");
        Self::set_console_capture_error(&shared, message.clone()).await;
        Self::notify_capture_ready(&mut ready_tx, Err(message));
        return;
      }
    };
    Self::notify_capture_ready(&mut ready_tx, Ok(()));

    let mut shutdown_rx = shutdown_rx;
    loop {
      let next_event = tokio::select! {
        _ = &mut shutdown_rx => {
          break;
        }
        event = stream.next() => event,
      };

      let Some(event) = next_event else {
        break;
      };

      let event = match event {
        Ok(event) => event,
        Err(error) => {
          Self::set_console_capture_error(
            &shared,
            format!("Camoufox console capture stream error for profile {profile_id}: {error}"),
          )
          .await;
          return;
        }
      };

      if let playwright::api::page::Event::Console(message) = event {
        let location = message.location().ok();
        let entry = ConsoleLogEntry {
          timestamp: Some(
            std::time::SystemTime::now()
              .duration_since(std::time::UNIX_EPOCH)
              .map(|duration| duration.as_secs_f64())
              .unwrap_or_default(),
          ),
          source: "console".to_string(),
          level: message.r#type().unwrap_or_else(|_| "log".to_string()),
          text: message.text().unwrap_or_default(),
          url: location.as_ref().map(|location| location.url.clone()),
          line_number: location.map(|location| location.line_number as i64),
        };
        Self::push_console_log(&shared, entry).await;
      }
    }

    shared.running.store(false, Ordering::SeqCst);
  }

  #[allow(dead_code)]
  async fn run_camoufox_network_capture(
    profile_id: String,
    page: Page,
    shared: NetworkCaptureShared,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ready_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
  ) {
    let mut ready_tx = Some(ready_tx);
    let mut stream = match page.subscribe_event() {
      Ok(stream) => stream,
      Err(error) => {
        let message =
          format!("Failed to subscribe Camoufox network capture for profile {profile_id}: {error}");
        Self::set_network_capture_error(&shared, message.clone()).await;
        Self::notify_capture_ready(&mut ready_tx, Err(message));
        return;
      }
    };
    Self::notify_capture_ready(&mut ready_tx, Ok(()));

    let mut request_ids: Vec<(PlaywrightRequest, String)> = Vec::new();
    let mut shutdown_rx = shutdown_rx;

    loop {
      let next_event = tokio::select! {
        _ = &mut shutdown_rx => {
          break;
        }
        event = stream.next() => event,
      };

      let Some(event) = next_event else {
        break;
      };

      let event = match event {
        Ok(event) => event,
        Err(error) => {
          Self::set_network_capture_error(
            &shared,
            format!("Camoufox network capture stream error for profile {profile_id}: {error}"),
          )
          .await;
          return;
        }
      };

      match event {
        playwright::api::page::Event::Request(request) => {
          let request_id = Uuid::new_v4().to_string();
          let url = request.url().unwrap_or_default();
          let method = request.method().unwrap_or_default();
          let resource_type = request.resource_type().ok();
          let initiator_type = Some(
            if request.is_navigation_request().unwrap_or(false) {
              "navigation"
            } else {
              "page"
            }
            .to_string(),
          );
          let start_timestamp = request
            .timing()
            .ok()
            .flatten()
            .map(|timing| timing.start_time / 1000.0)
            .or_else(|| {
              std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|duration| duration.as_secs_f64())
            });
          let request_headers =
            Self::playwright_headers_to_json(request.headers().unwrap_or_default());
          let post_data = request
            .post_data()
            .ok()
            .flatten()
            .map(|body| String::from_utf8_lossy(&body).to_string());

          request_ids.push((request.clone(), request_id.clone()));
          Self::update_network_request(&shared, &request_id, move |entry| {
            entry.url = url;
            entry.method = method;
            entry.resource_type = resource_type;
            entry.initiator_type = initiator_type;
            entry.start_timestamp = start_timestamp;
            entry.request_headers = request_headers;
            entry.post_data = post_data;
            entry.failed = false;
            entry.error_text = None;
            entry.blocked_reason = None;
            entry.has_response_body = false;
          })
          .await;
        }
        playwright::api::page::Event::Response(response) => {
          let request = response.request();
          let request_id =
            Self::camoufox_find_request_id(&request_ids, &request).unwrap_or_else(|| {
              let request_id = Uuid::new_v4().to_string();
              request_ids.push((request.clone(), request_id.clone()));
              request_id
            });
          let status = response.status().ok().map(|value| value as i64);
          let status_text = response.status_text().ok();
          let headers_vec = response.headers().await.unwrap_or_default();
          let response_headers = Self::playwright_headers_to_json(
            headers_vec
              .into_iter()
              .map(|header| (header.name, header.value)),
          );
          let mime_type = Self::response_mime_type_from_headers(&response_headers);
          let url = response.url().unwrap_or_default();

          Self::update_network_request(&shared, &request_id, move |entry| {
            entry.status = status;
            entry.status_text = status_text;
            entry.response_headers = response_headers;
            entry.mime_type = mime_type;
            if entry.url.is_empty() {
              entry.url = url;
            }
          })
          .await;
        }
        playwright::api::page::Event::RequestFinished(request) => {
          let request_id = match Self::camoufox_find_request_id(&request_ids, &request) {
            Some(request_id) => request_id,
            None => continue,
          };
          let response = request.response().await.ok().flatten();
          let response_end_ms = request.response_end().ok().flatten();
          let end_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_secs_f64());

          Self::update_network_request(&shared, &request_id, move |entry| {
            entry.end_timestamp = end_timestamp;
            entry.duration_ms = response_end_ms.or({
              match (entry.start_timestamp, end_timestamp) {
                (Some(start), Some(end)) if end >= start => Some((end - start) * 1000.0),
                _ => None,
              }
            });
          })
          .await;

          if let Some(response) = response {
            match response.body().await {
              Ok(body) => {
                let (body, base64_encoded) = match String::from_utf8(body.clone()) {
                  Ok(text) => (text, false),
                  Err(_) => (base64::engine::general_purpose::STANDARD.encode(body), true),
                };
                Self::set_network_request_body(&shared, &request_id, body, base64_encoded).await;
              }
              Err(error) => {
                Self::update_network_request(&shared, &request_id, move |entry| {
                  entry.has_response_body = false;
                  entry.error_text.get_or_insert_with(|| error.to_string());
                })
                .await;
              }
            }
          }
        }
        playwright::api::page::Event::RequestFailed(request) => {
          let request_id = match Self::camoufox_find_request_id(&request_ids, &request) {
            Some(request_id) => request_id,
            None => continue,
          };
          let error_text = request.failure().ok().flatten();
          let end_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_secs_f64());
          Self::update_network_request(&shared, &request_id, move |entry| {
            entry.failed = true;
            entry.error_text = error_text;
            entry.end_timestamp = end_timestamp;
          })
          .await;
        }
        _ => {}
      }
    }

    shared.running.store(false, Ordering::SeqCst);
  }

  async fn run_network_capture(
    profile_id: String,
    ws_url: String,
    shared: NetworkCaptureShared,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    mut body_request_rx: mpsc::Receiver<BodyFetchRequest>,
    ready_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
    timeout_ms: u64,
  ) {
    use futures_util::sink::SinkExt;
    use futures_util::stream::StreamExt;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    let timeout = std::time::Duration::from_millis(timeout_ms.max(1_000));
    let connect_result = tokio::time::timeout(timeout, connect_async(&ws_url)).await;
    let mut ready_tx = Some(ready_tx);
    let (mut ws_stream, _) = match connect_result {
      Ok(Ok(result)) => result,
      Ok(Err(e)) => {
        let message = format!("Failed to connect network capture for profile {profile_id}: {e}");
        Self::set_network_capture_error(&shared, message.clone()).await;
        Self::notify_capture_ready(&mut ready_tx, Err(message));
        return;
      }
      Err(_) => {
        let message = format!(
          "Timed out connecting network capture for profile {profile_id} after {timeout_ms}ms"
        );
        Self::set_network_capture_error(&shared, message.clone()).await;
        Self::notify_capture_ready(&mut ready_tx, Err(message));
        return;
      }
    };

    let command = serde_json::json!({
      "id": 1_u64,
      "method": "Network.enable",
      "params": {}
    });
    if let Err(e) = ws_stream
      .send(Message::Text(command.to_string().into()))
      .await
    {
      let message = format!("Failed to initialize network capture for profile {profile_id}: {e}");
      Self::set_network_capture_error(&shared, message.clone()).await;
      Self::notify_capture_ready(&mut ready_tx, Err(message));
      return;
    }

    let mut shutdown_rx = shutdown_rx;
    loop {
      let next_message = tokio::select! {
        _ = &mut shutdown_rx => {
          shared.running.store(false, Ordering::SeqCst);
          Self::notify_capture_ready(
            &mut ready_tx,
            Err(format!("Network capture stopped before initialization for profile {profile_id}")),
          );
          return;
        }
        message = tokio::time::timeout(timeout, ws_stream.next()) => message,
      };

      let Some(message) = (match next_message {
        Ok(message) => message,
        Err(_) => {
          let message = format!("Timed out enabling network capture for profile {profile_id}");
          Self::set_network_capture_error(&shared, message.clone()).await;
          Self::notify_capture_ready(&mut ready_tx, Err(message));
          return;
        }
      }) else {
        let message =
          format!("Network capture stream closed while initializing profile {profile_id}");
        Self::set_network_capture_error(&shared, message.clone()).await;
        Self::notify_capture_ready(&mut ready_tx, Err(message));
        return;
      };

      let message = match message {
        Ok(message) => message,
        Err(e) => {
          let message = format!("Network capture stream error for profile {profile_id}: {e}");
          Self::set_network_capture_error(&shared, message.clone()).await;
          Self::notify_capture_ready(&mut ready_tx, Err(message));
          return;
        }
      };

      if let Message::Text(text) = message {
        let response: serde_json::Value = match serde_json::from_str(text.as_str()) {
          Ok(response) => response,
          Err(_) => continue,
        };

        if response.get("id") == Some(&serde_json::json!(1_u64)) {
          if let Some(error) = response.get("error") {
            let message =
              format!("CDP error enabling network capture for profile {profile_id}: {error}");
            Self::set_network_capture_error(&shared, message.clone()).await;
            Self::notify_capture_ready(&mut ready_tx, Err(message));
            return;
          }
          break;
        }
      }
    }
    Self::notify_capture_ready(&mut ready_tx, Ok(()));

    let mut pending_body_requests: PendingBodyFetchMap = HashMap::new();
    let mut next_command_id = 2_u64;

    loop {
      let next_message = tokio::select! {
        _ = &mut shutdown_rx => {
          break;
        }
        body_request = body_request_rx.recv() => {
          match body_request {
            Some(body_request) => {
              let command_id = next_command_id;
              next_command_id = next_command_id.saturating_add(1);
              let command = serde_json::json!({
                "id": command_id,
                "method": "Network.getResponseBody",
                "params": {
                  "requestId": body_request.request_id
                }
              });
              if let Err(error) = ws_stream.send(Message::Text(command.to_string().into())).await {
                let _ = body_request.response_tx.send(Err(format!(
                  "Failed to request response body: {error}"
                )));
              } else {
                pending_body_requests
                  .insert(command_id, (body_request.request_id, body_request.response_tx));
              }
              continue;
            }
            None => break,
          }
        }
        message = ws_stream.next() => message,
      };

      let Some(message) = next_message else {
        Self::fail_pending_body_requests(
          &mut pending_body_requests,
          "network capture stream closed",
        )
        .await;
        break;
      };

      let message = match message {
        Ok(message) => message,
        Err(e) => {
          Self::fail_pending_body_requests(
            &mut pending_body_requests,
            format!("network capture stream error: {e}"),
          )
          .await;
          Self::set_network_capture_error(
            &shared,
            format!("Network capture stream error for profile {profile_id}: {e}"),
          )
          .await;
          return;
        }
      };

      let Message::Text(text) = message else {
        continue;
      };

      let payload: serde_json::Value = match serde_json::from_str(text.as_str()) {
        Ok(payload) => payload,
        Err(_) => continue,
      };

      if let Some(command_id) = payload.get("id").and_then(|value| value.as_u64()) {
        if let Some((request_id, response_tx)) = pending_body_requests.remove(&command_id) {
          if let Some(error) = payload.get("error") {
            let _ = response_tx.send(Err(format!(
              "Response body is unavailable for request {request_id}: {error}"
            )));
          } else {
            let result = payload
              .get("result")
              .cloned()
              .unwrap_or_else(|| serde_json::json!({}));
            let body = result
              .get("body")
              .and_then(|value| value.as_str())
              .unwrap_or_default()
              .to_string();
            let base64_encoded = result
              .get("base64Encoded")
              .and_then(|value| value.as_bool())
              .unwrap_or(false);
            Self::set_network_request_body(&shared, &request_id, body.clone(), base64_encoded)
              .await;
            let _ = response_tx.send(Ok((body, base64_encoded)));
          }
          continue;
        }
      }

      Self::handle_network_capture_payload(&shared, payload).await;
    }

    Self::fail_pending_body_requests(&mut pending_body_requests, "network capture stopped").await;
    shared.running.store(false, Ordering::SeqCst);
  }

  pub fn get_port(&self) -> Option<u16> {
    let port = self.port.load(Ordering::SeqCst);
    if port > 0 {
      Some(port)
    } else {
      None
    }
  }

  pub async fn start(&self, app_handle: AppHandle) -> Result<u16, String> {
    if self.is_running() {
      return Err("MCP server is already running".to_string());
    }

    let settings_manager = SettingsManager::instance();
    let settings = settings_manager
      .load_settings()
      .map_err(|e| format!("Failed to load settings: {e}"))?;

    // Get or generate token
    let existing_token = settings_manager
      .get_mcp_token(&app_handle)
      .await
      .ok()
      .flatten();

    let token = if let Some(t) = existing_token {
      t
    } else {
      settings_manager
        .generate_mcp_token(&app_handle)
        .await
        .map_err(|e| format!("Failed to generate MCP token: {e}"))?
    };

    // Determine port (use saved port, or try default, or random)
    let preferred_port = settings.mcp_port.unwrap_or(DEFAULT_MCP_PORT);
    let actual_port = self.bind_to_available_port(preferred_port).await?;

    // Save port if it changed
    if settings.mcp_port != Some(actual_port) {
      let mut new_settings = settings;
      new_settings.mcp_port = Some(actual_port);
      settings_manager
        .save_settings(&new_settings)
        .map_err(|e| format!("Failed to save settings: {e}"))?;
    }

    // Store state
    let mut inner = self.inner.lock().await;
    inner.app_handle = Some(app_handle);
    inner.token = Some(token.clone());

    // Create shutdown channel
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    inner.shutdown_tx = Some(shutdown_tx);

    self.port.store(actual_port, Ordering::SeqCst);
    self.is_running.store(true, Ordering::SeqCst);

    // Start HTTP server in background
    let http_state = McpHttpState {
      server: McpServer::instance(),
      token,
    };
    tokio::spawn(Self::run_http_server(actual_port, http_state, shutdown_rx));

    log::info!("[mcp] Server started on port {}", actual_port);
    Ok(actual_port)
  }

  async fn bind_to_available_port(&self, preferred: u16) -> Result<u16, String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], preferred));
    if TcpListener::bind(addr).await.is_ok() {
      return Ok(preferred);
    }

    for _ in 0..10 {
      let port = 51000 + (rand::random::<u16>() % 1000);
      let addr = SocketAddr::from(([127, 0, 0, 1], port));
      if TcpListener::bind(addr).await.is_ok() {
        return Ok(port);
      }
    }

    Err("Could not find available port for MCP server".to_string())
  }

  async fn run_http_server(
    port: u16,
    state: McpHttpState,
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,
  ) {
    let app = Router::new()
      .route(
        "/mcp/{token}",
        post(Self::handle_mcp_post)
          .get(Self::handle_mcp_get)
          .delete(Self::handle_mcp_delete),
      )
      .route(
        "/mcp",
        post(Self::handle_mcp_post)
          .get(Self::handle_mcp_get)
          .delete(Self::handle_mcp_delete),
      )
      .route("/health", get(Self::handle_health))
      .layer(middleware::from_fn_with_state(
        state.clone(),
        Self::auth_middleware,
      ))
      .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));

    let server = async {
      match TcpListener::bind(addr).await {
        Ok(listener) => {
          log::info!("[mcp] Server listening on http://127.0.0.1:{}/mcp", port);
          if let Err(e) = axum::serve(listener, app).await {
            log::error!("[mcp] Server error: {}", e);
          }
        }
        Err(e) => {
          log::error!("[mcp] Failed to bind on port {}: {}", port, e);
        }
      }
    };

    tokio::select! {
      _ = server => {},
      _ = shutdown_rx => {
        log::info!("[mcp] Server shutting down");
      },
    }
  }

  async fn auth_middleware(
    State(state): State<McpHttpState>,
    req: Request<Body>,
    next: Next,
  ) -> Result<Response, StatusCode> {
    let path = req.uri().path();

    if path == "/health" {
      return Ok(next.run(req).await);
    }

    // Check token from URL path: /mcp/{token}
    let path_token = path
      .strip_prefix("/mcp/")
      .filter(|t| !t.is_empty() && !t.contains('/'));

    // Check token from Authorization header
    let header_token = req
      .headers()
      .get(header::AUTHORIZATION)
      .and_then(|h| h.to_str().ok())
      .and_then(|h| h.strip_prefix("Bearer "));

    let valid =
      path_token == Some(state.token.as_str()) || header_token == Some(state.token.as_str());

    if !valid {
      return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(req).await)
  }

  async fn handle_health() -> impl IntoResponse {
    Json(serde_json::json!({
      "status": "ok",
      "server": SERVER_NAME,
      "version": SERVER_VERSION,
      "protocolVersion": PROTOCOL_VERSION,
    }))
  }

  async fn handle_mcp_get() -> impl IntoResponse {
    // We don't support server-initiated SSE streams
    StatusCode::METHOD_NOT_ALLOWED
  }

  async fn handle_mcp_delete(
    State(state): State<McpHttpState>,
    req: Request<Body>,
  ) -> impl IntoResponse {
    let session_id = req
      .headers()
      .get("mcp-session-id")
      .and_then(|h| h.to_str().ok())
      .map(|s| s.to_string());

    if let Some(sid) = session_id {
      let mut inner = state.server.inner.lock().await;
      inner.sessions.remove(&sid);
      log::info!("[mcp] Session terminated: {}", sid);
    }

    StatusCode::OK
  }

  async fn handle_mcp_post(State(state): State<McpHttpState>, req: Request<Body>) -> Response {
    let session_id = req
      .headers()
      .get("mcp-session-id")
      .and_then(|h| h.to_str().ok())
      .map(|s| s.to_string());

    let body_bytes = match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
      Ok(b) => b,
      Err(_) => {
        return (StatusCode::BAD_REQUEST, "Invalid request body").into_response();
      }
    };

    let request: McpRequest = match serde_json::from_slice(&body_bytes) {
      Ok(r) => r,
      Err(_) => {
        return (StatusCode::BAD_REQUEST, "Invalid JSON").into_response();
      }
    };

    let is_notification = request.id.is_none();
    let method = request.method.clone();

    // Handle initialize (no session required)
    if method == "initialize" {
      let response = state.server.handle_initialize(request).await;
      match response {
        Ok((session_id, result)) => {
          let body = McpResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(result.0),
            result: Some(result.1),
            error: None,
          };
          Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .header("mcp-session-id", &session_id)
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
        }
        Err((id, error)) => {
          let body = McpResponse {
            jsonrpc: "2.0".to_string(),
            id: Some(id),
            result: None,
            error: Some(error),
          };
          Json(body).into_response()
        }
      }
    } else if is_notification {
      // Notifications (like notifications/initialized) -> 202 Accepted
      if method == "notifications/initialized" {
        if let Some(sid) = &session_id {
          let mut inner = state.server.inner.lock().await;
          if let Some(session) = inner.sessions.get_mut(sid) {
            session.initialized = true;
          }
        }
      }
      StatusCode::ACCEPTED.into_response()
    } else {
      // Validate session exists
      if let Some(sid) = &session_id {
        let inner = state.server.inner.lock().await;
        if !inner.sessions.contains_key(sid) {
          return StatusCode::NOT_FOUND.into_response();
        }
      }

      let response = state.server.handle_request(request).await;
      Json(response).into_response()
    }
  }

  pub async fn stop(&self) -> Result<(), String> {
    if !self.is_running() {
      return Err("MCP server is not running".to_string());
    }

    let mut inner = self.inner.lock().await;
    inner.app_handle = None;
    inner.token = None;
    inner.sessions.clear();
    inner.selected_tabs.clear();

    for capture in inner.console_captures.values_mut() {
      if let Some(tx) = capture.shutdown_tx.take() {
        let _ = tx.send(());
      }
      capture.shared.running.store(false, Ordering::SeqCst);
    }
    inner.console_captures.clear();

    for capture in inner.network_captures.values_mut() {
      if let Some(tx) = capture.shutdown_tx.take() {
        let _ = tx.send(());
      }
      capture.shared.running.store(false, Ordering::SeqCst);
    }
    inner.network_captures.clear();
    inner.downloads.clear();

    // Send shutdown signal
    if let Some(tx) = inner.shutdown_tx.take() {
      let _ = tx.send(());
    }

    self.port.store(0, Ordering::SeqCst);
    self.is_running.store(false, Ordering::SeqCst);

    log::info!("[mcp] Server stopped");
    Ok(())
  }

  pub fn get_tools(&self) -> Vec<McpTool> {
    vec![
      McpTool {
        name: "list_profiles".to_string(),
        description: "List all Chromium and Camoufox browser profiles".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "get_profile".to_string(),
        description: "Get details of a specific browser profile".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile to retrieve"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "run_profile".to_string(),
        description: "Launch a browser profile with an optional URL".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile to launch"
            },
            "url": {
              "type": "string",
              "description": "Optional URL to open in the browser"
            },
            "headless": {
              "type": "boolean",
              "description": "Run the browser in headless mode"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "kill_profile".to_string(),
        description: "Stop a running browser profile".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile to stop"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "create_profile".to_string(),
        description: "Create a new browser profile".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "name": {
              "type": "string",
              "description": "Name for the new profile"
            },
            "browser": {
              "type": "string",
              "enum": ["chromium", "camoufox"],
              "description": "Browser engine to use"
            },
            "proxy_id": {
              "type": "string",
              "description": "Optional proxy UUID to assign"
            },
            "launch_hook": {
              "type": "string",
              "description": "Optional HTTP(S) URL to call before launch for transient proxy overrides"
            },
            "group_id": {
              "type": "string",
              "description": "Optional group UUID to assign"
            },
            "tags": {
              "type": "array",
              "items": { "type": "string" },
              "description": "Optional tags for the profile"
            }
          },
          "required": ["name", "browser"]
        }),
      },
      McpTool {
        name: "update_profile".to_string(),
        description: "Update an existing browser profile's settings".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile to update"
            },
            "name": {
              "type": "string",
              "description": "New name for the profile"
            },
            "proxy_id": {
              "type": "string",
              "description": "Proxy UUID to assign (empty string to remove)"
            },
            "launch_hook": {
              "type": "string",
              "description": "Launch hook URL to assign (empty string to remove)"
            },
            "group_id": {
              "type": "string",
              "description": "Group UUID to assign (empty string to remove)"
            },
            "tags": {
              "type": "array",
              "items": { "type": "string" },
              "description": "Tags for the profile (replaces existing tags)"
            },
            "extension_group_id": {
              "type": "string",
              "description": "Extension group UUID to assign (empty string to remove)"
            },
            "proxy_bypass_rules": {
              "type": "array",
              "items": { "type": "string" },
              "description": "Proxy bypass rules (replaces existing rules)"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "delete_profile".to_string(),
        description: "Delete a browser profile and all its data".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile to delete"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "list_tags".to_string(),
        description: "List all tags used across profiles".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "list_proxies".to_string(),
        description: "List all configured proxies".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "get_profile_status".to_string(),
        description: "Check if a browser profile is currently running".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile to check"
            }
          },
          "required": ["profile_id"]
        }),
      },
      // Group management tools
      McpTool {
        name: "list_groups".to_string(),
        description: "List all profile groups".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "get_group".to_string(),
        description: "Get details of a specific group".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "group_id": {
              "type": "string",
              "description": "The UUID of the group to retrieve"
            }
          },
          "required": ["group_id"]
        }),
      },
      McpTool {
        name: "create_group".to_string(),
        description: "Create a new profile group".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "name": {
              "type": "string",
              "description": "The name for the new group"
            }
          },
          "required": ["name"]
        }),
      },
      McpTool {
        name: "update_group".to_string(),
        description: "Update an existing group's name".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "group_id": {
              "type": "string",
              "description": "The UUID of the group to update"
            },
            "name": {
              "type": "string",
              "description": "The new name for the group"
            }
          },
          "required": ["group_id", "name"]
        }),
      },
      McpTool {
        name: "delete_group".to_string(),
        description: "Delete a profile group".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "group_id": {
              "type": "string",
              "description": "The UUID of the group to delete"
            }
          },
          "required": ["group_id"]
        }),
      },
      McpTool {
        name: "assign_profiles_to_group".to_string(),
        description: "Assign one or more profiles to a group".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_ids": {
              "type": "array",
              "items": { "type": "string" },
              "description": "Array of profile UUIDs to assign"
            },
            "group_id": {
              "type": "string",
              "description": "The UUID of the group to assign to (null to remove from group)"
            }
          },
          "required": ["profile_ids"]
        }),
      },
      // Full proxy management tools
      McpTool {
        name: "get_proxy".to_string(),
        description: "Get details of a specific proxy".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "proxy_id": {
              "type": "string",
              "description": "The UUID of the proxy to retrieve"
            }
          },
          "required": ["proxy_id"]
        }),
      },
      McpTool {
        name: "create_proxy".to_string(),
        description: "Create a new proxy configuration.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "name": {
              "type": "string",
              "description": "The name for the new proxy"
            },
            "proxy_type": {
              "type": "string",
              "enum": ["http", "https", "socks4", "socks5"],
              "description": "The type of proxy (for regular proxies)"
            },
            "host": {
              "type": "string",
              "description": "The proxy host address (for regular proxies)"
            },
            "port": {
              "type": "integer",
              "description": "The proxy port number (for regular proxies)"
            },
            "username": {
              "type": "string",
              "description": "Optional username for authentication (for regular proxies)"
            },
            "password": {
              "type": "string",
              "description": "Optional password for authentication (for regular proxies)"
            }
          },
          "required": ["name", "proxy_type", "host", "port"]
        }),
      },
      McpTool {
        name: "update_proxy".to_string(),
        description: "Update an existing proxy configuration".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "proxy_id": {
              "type": "string",
              "description": "The UUID of the proxy to update"
            },
            "name": {
              "type": "string",
              "description": "New name for the proxy"
            },
            "proxy_type": {
              "type": "string",
              "enum": ["http", "https", "socks4", "socks5"],
              "description": "The type of proxy (for regular proxies)"
            },
            "host": {
              "type": "string",
              "description": "The proxy host address (for regular proxies)"
            },
            "port": {
              "type": "integer",
              "description": "The proxy port number (for regular proxies)"
            },
            "username": {
              "type": "string",
              "description": "Optional username for authentication (for regular proxies)"
            },
            "password": {
              "type": "string",
              "description": "Optional password for authentication (for regular proxies)"
            }
          },
          "required": ["proxy_id"]
        }),
      },
      McpTool {
        name: "delete_proxy".to_string(),
        description: "Delete a proxy configuration".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "proxy_id": {
              "type": "string",
              "description": "The UUID of the proxy to delete"
            }
          },
          "required": ["proxy_id"]
        }),
      },
      McpTool {
        name: "export_proxies".to_string(),
        description: "Export all proxy configurations".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "format": {
              "type": "string",
              "enum": ["json", "txt"],
              "description": "Export format (json for structured data, txt for URL format)"
            }
          },
          "required": ["format"]
        }),
      },
      McpTool {
        name: "import_proxies".to_string(),
        description: "Import proxy configurations from JSON or TXT content".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "content": {
              "type": "string",
              "description": "The proxy configuration content to import"
            },
            "format": {
              "type": "string",
              "enum": ["json", "txt"],
              "description": "Import format (json or txt)"
            },
            "name_prefix": {
              "type": "string",
              "description": "Optional prefix for imported proxy names (default: 'Imported')"
            }
          },
          "required": ["content", "format"]
        }),
      },
      // VPN management tools
      McpTool {
        name: "import_vpn".to_string(),
        description: "Import a WireGuard (.conf) configuration".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "content": {
              "type": "string",
              "description": "Raw WireGuard config file content"
            },
            "filename": {
              "type": "string",
              "description": "Original filename (.conf)"
            },
            "name": {
              "type": "string",
              "description": "Optional display name for the VPN config"
            }
          },
          "required": ["content", "filename"]
        }),
      },
      McpTool {
        name: "list_vpn_configs".to_string(),
        description: "List all stored VPN configurations".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "delete_vpn".to_string(),
        description: "Delete a VPN configuration".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "vpn_id": {
              "type": "string",
              "description": "The UUID of the VPN config to delete"
            }
          },
          "required": ["vpn_id"]
        }),
      },
      McpTool {
        name: "connect_vpn".to_string(),
        description: "Connect to a VPN configuration".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "vpn_id": {
              "type": "string",
              "description": "The UUID of the VPN config to connect"
            }
          },
          "required": ["vpn_id"]
        }),
      },
      McpTool {
        name: "disconnect_vpn".to_string(),
        description: "Disconnect from a VPN".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "vpn_id": {
              "type": "string",
              "description": "The UUID of the VPN to disconnect"
            }
          },
          "required": ["vpn_id"]
        }),
      },
      McpTool {
        name: "get_vpn_status".to_string(),
        description: "Get the connection status of a VPN".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "vpn_id": {
              "type": "string",
              "description": "The UUID of the VPN to check"
            }
          },
          "required": ["vpn_id"]
        }),
      },
      // Fingerprint management tools
      McpTool {
        name: "get_profile_fingerprint".to_string(),
        description: "Get the fingerprint configuration for a Chromium or Camoufox profile"
          .to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "update_profile_fingerprint".to_string(),
        description: "Update the fingerprint configuration for a Chromium or Camoufox profile."
          .to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile to update"
            },
            "fingerprint": {
              "type": "string",
              "description": "JSON string of the fingerprint configuration, or null to clear"
            },
            "os": {
              "type": "string",
              "enum": ["windows", "macos", "linux"],
              "description": "Operating system for fingerprint generation"
            },
            "randomize_fingerprint_on_launch": {
              "type": "boolean",
              "description": "Whether to generate a new fingerprint on every launch"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "update_profile_proxy_bypass_rules".to_string(),
        description:
          "Update proxy bypass rules for a profile. Requests matching these rules will connect directly, bypassing the proxy."
            .to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile to update"
            },
            "rules": {
              "type": "array",
              "items": { "type": "string" },
              "description": "Array of bypass rules. Supports hostnames (e.g. 'example.com'), IP addresses, and regex patterns."
            }
          },
          "required": ["profile_id", "rules"]
        }),
      },
      McpTool {
        name: "update_profile_dns_blocklist".to_string(),
        description:
          "Update the DNS blocklist level for a profile. Blocks ads, trackers, and malware domains at the proxy level."
            .to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile to update"
            },
            "level": {
              "type": "string",
              "enum": ["none", "light", "normal", "pro", "pro_plus", "ultimate"],
              "description": "DNS blocklist level. 'none' disables blocking."
            }
          },
          "required": ["profile_id", "level"]
        }),
      },
      McpTool {
        name: "get_dns_blocklist_status".to_string(),
        description: "Get the cache status of all DNS blocklist tiers including entry counts and freshness.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "list_extensions".to_string(),
        description: "List all managed browser extensions.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "list_extension_groups".to_string(),
        description: "List all extension groups.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "create_extension_group".to_string(),
        description: "Create a new extension group.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "name": { "type": "string", "description": "Name for the extension group" }
          },
          "required": ["name"]
        }),
      },
      McpTool {
        name: "delete_extension".to_string(),
        description: "Delete a managed extension.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "extension_id": { "type": "string", "description": "The extension ID to delete" }
          },
          "required": ["extension_id"]
        }),
      },
      McpTool {
        name: "delete_extension_group".to_string(),
        description: "Delete an extension group.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "group_id": { "type": "string", "description": "The extension group ID to delete" }
          },
          "required": ["group_id"]
        }),
      },
      McpTool {
        name: "assign_extension_group_to_profile".to_string(),
        description: "Assign an extension group to a profile, or remove the assignment.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string", "description": "The profile ID" },
            "extension_group_id": { "type": "string", "description": "The extension group ID, or empty string to remove" }
          },
          "required": ["profile_id"]
        }),
      },
      // Team lock tools
      McpTool {
        name: "get_team_locks".to_string(),
        description: "List all active team profile locks. Requires team plan.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "get_team_lock_status".to_string(),
        description: "Check if a profile is locked by a team member. Requires team plan.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile to check"
            }
          },
          "required": ["profile_id"]
        }),
      },
      // Sync settings tools
      McpTool {
        name: "get_sync_settings".to_string(),
        description: "Read the configured self-hosted sync server URL and sync token.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "save_sync_settings".to_string(),
        description: "Save or clear the self-hosted sync server URL and sync token.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "sync_server_url": {
              "type": ["string", "null"],
              "description": "Sync server base URL. Pass null to clear."
            },
            "sync_token": {
              "type": ["string", "null"],
              "description": "Sync bearer token. Pass null to clear."
            }
          },
          "required": []
        }),
      },
      McpTool {
        name: "request_profile_sync".to_string(),
        description: "Queue an immediate sync for a sync-enabled profile.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile to sync"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "validate_recipe".to_string(),
        description: "Validate a JSON recipe/workflow definition against the current MCP tool catalog and basic argument shape rules.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "recipe": {
              "type": "object",
              "description": "Recipe definition with version, name, optional defaults/inputs, and ordered steps.",
              "properties": {
                "version": { "type": "integer", "description": "Recipe version. Must be 1." },
                "name": { "type": "string", "description": "Human-readable recipe name." },
                "description": { "type": "string", "description": "Optional description." },
                "defaults": {
                  "type": "object",
                  "properties": {
                    "timeout_ms": { "type": "integer", "description": "Optional shared timeout override." },
                    "auto_wait": { "type": "boolean", "description": "Optional shared auto-wait override." }
                  }
                },
                "inputs": {
                  "type": ["object", "array", "string", "number", "boolean", "null"],
                  "description": "Optional future-facing input declaration block."
                },
                "steps": {
                  "type": "array",
                  "description": "Ordered recipe steps.",
                  "items": {
                    "type": "object",
                    "properties": {
                      "id": { "type": "string", "description": "Stable step identifier." },
                      "tool": { "type": "string", "description": "MCP tool name to invoke." },
                      "arguments": { "type": "object", "description": "Tool argument object." },
                      "condition": { "description": "Optional execution condition payload." },
                      "output": { "type": "string", "description": "Optional named output alias." },
                      "on_error": { "type": "string", "enum": ["stop", "continue"], "description": "Step failure policy. Default is stop." },
                      "retry": {
                        "type": "object",
                        "description": "Optional retry policy.",
                        "properties": {
                          "max_attempts": { "type": "integer", "description": "Retry attempt count. Must be >= 1." },
                          "delay_ms": { "type": "integer", "description": "Optional delay between attempts." }
                        },
                        "required": ["max_attempts"]
                      }
                    },
                    "required": ["id", "tool"]
                  }
                }
              },
              "required": ["version", "name", "steps"]
            }
          },
          "required": ["recipe"]
        }),
      },
      McpTool {
        name: "preview_recipe".to_string(),
        description: "Preview a normalized recipe/workflow with merged defaults, retry safety hints, and replay-oriented step metadata.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "recipe": {
              "type": "object",
              "description": "Recipe definition. Same shape as validate_recipe."
            }
          },
          "required": ["recipe"]
        }),
      },
      McpTool {
        name: "run_recipe".to_string(),
        description: "Run a validated single-profile recipe/workflow and return structured step execution records, failure reasons, and replay metadata.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "recipe": {
              "type": "object",
              "description": "Recipe definition. Same shape as validate_recipe."
            },
            "inputs": {
              "type": "object",
              "description": "Optional runtime inputs available to placeholders such as {{inputs.profile_id}}."
            }
          },
          "required": ["recipe"]
        }),
      },
      McpTool {
        name: "run_batch_profile_workflow".to_string(),
        description: "Run the same recipe/workflow across multiple profiles, aggregate structured per-profile reports, and optionally stop when the first profile fails.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_ids": {
              "type": "array",
              "items": { "type": "string" },
              "description": "Profiles that should each receive one independent run of the recipe."
            },
            "recipe": {
              "type": "object",
              "description": "Recipe definition. Same shape as validate_recipe."
            },
            "inputs": {
              "type": "object",
              "description": "Optional shared inputs merged into every profile run."
            },
            "max_concurrency": {
              "type": "integer",
              "description": "Maximum number of profile runs executed in parallel. Default is 1."
            },
            "stop_on_profile_failure": {
              "type": "boolean",
              "description": "When true, stop scheduling new profiles after the first non-success profile report. Default is false."
            },
            "per_profile_inputs": {
              "type": "object",
              "description": "Optional per-profile input overrides keyed by profile_id.",
              "additionalProperties": {
                "type": "object"
              }
            }
          },
          "required": ["profile_ids", "recipe"]
        }),
      },
      // Synchronizer tools
      McpTool {
        name: "start_sync_session".to_string(),
        description: "Start a synchronizer session. Launches a leader profile and follower profiles, then mirrors all actions from the leader to the followers in real time. Only Chromium profiles are supported.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "leader_profile_id": {
              "type": "string",
              "description": "The UUID of the leader profile"
            },
            "follower_profile_ids": {
              "type": "array",
              "items": { "type": "string" },
              "description": "UUIDs of follower profiles"
            }
          },
          "required": ["leader_profile_id", "follower_profile_ids"]
        }),
      },
      McpTool {
        name: "stop_sync_session".to_string(),
        description: "Stop an active synchronizer session. Kills all follower profiles and the leader.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "session_id": {
              "type": "string",
              "description": "The sync session ID"
            }
          },
          "required": ["session_id"]
        }),
      },
      McpTool {
        name: "get_sync_sessions".to_string(),
        description: "List all active synchronizer sessions.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {}
        }),
      },
      McpTool {
        name: "remove_sync_follower".to_string(),
        description: "Remove a follower from an active synchronizer session.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "session_id": {
              "type": "string",
              "description": "The sync session ID"
            },
            "follower_profile_id": {
              "type": "string",
              "description": "The UUID of the follower to remove"
            }
          },
          "required": ["session_id", "follower_profile_id"]
        }),
      },
      // Action recorder tools
      McpTool {
        name: "start_action_recording".to_string(),
        description: "Start recording user interactions on a running Chromium or Camoufox profile. Events are captured via an injected content script and buffered until stop_action_recording is called.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "UUID of a running Chromium or Camoufox profile"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "stop_action_recording".to_string(),
        description: "Stop an active action recording session and persist the captured event stream to disk.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "session_id": {
              "type": "string",
              "description": "Recording session id returned by start_action_recording"
            }
          },
          "required": ["session_id"]
        }),
      },
      McpTool {
        name: "get_recorded_events".to_string(),
        description: "List active action-recording sessions (in-progress only).".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {}
        }),
      },
      McpTool {
        name: "list_recordings".to_string(),
        description: "List saved action recordings stored on disk (summaries only).".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {}
        }),
      },
      McpTool {
        name: "export_recording_as_recipe".to_string(),
        description: "Export a saved action recording as an MCP recipe JSON that can be executed with run_recipe / run_batch_profile_workflow.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "recording_id": {
              "type": "string",
              "description": "Saved recording id"
            },
            "recipe_name": {
              "type": "string",
              "description": "Optional recipe name override"
            }
          },
          "required": ["recording_id"]
        }),
      },
      McpTool {
        name: "replay_recording".to_string(),
        description: "Replay a saved raw action recording on a running profile using coordinate/keystroke-level dispatch.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "recording_id": {
              "type": "string",
              "description": "Saved recording id"
            },
            "profile_id": {
              "type": "string",
              "description": "UUID of a running profile to replay onto"
            }
          },
          "required": ["recording_id", "profile_id"]
        }),
      },
      // Browser interaction tools
      McpTool {
        name: "navigate".to_string(),
        description: "Navigate a running browser profile to a URL. Waits for the page to fully load before returning.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the running profile"
            },
            "url": {
              "type": "string",
              "description": "The URL to navigate to"
            }
          },
          "required": ["profile_id", "url"]
        }),
      },
      McpTool {
        name: "screenshot".to_string(),
        description: "Take a screenshot of the current page in a running browser profile. Returns base64-encoded image."
          .to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the running profile"
            },
            "format": {
              "type": "string",
              "enum": ["png", "jpeg", "webp"],
              "description": "Image format (default: png)"
            },
            "quality": {
              "type": "integer",
              "description": "Image quality 0-100 for jpeg/webp (default: 80)"
            },
            "full_page": {
              "type": "boolean",
              "description": "Capture the full scrollable page (default: false)"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "evaluate_javascript".to_string(),
        description:
          "Execute JavaScript in the context of the current page or a selected frame and return the result. Works with both static and dynamically-generated content. Set wait_for_load=true if the script triggers navigation (e.g., form.submit())."
            .to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the running profile"
            },
            "expression": {
              "type": "string",
              "description": "JavaScript expression to evaluate"
            },
            "frame": Self::frame_ref_input_schema("Optional frame selector. If provided, JavaScript runs inside that frame's execution context."),
            "await_promise": {
              "type": "boolean",
              "description": "Whether to await the result if it's a Promise (default: false)"
            },
            "wait_for_load": {
              "type": "boolean",
              "description": "Wait for page load after execution, use when the script triggers navigation like form.submit() (default: false)"
            }
          },
          "required": ["profile_id", "expression"]
        }),
      },
      McpTool {
        name: "click_element".to_string(),
        description: "Click on an element identified by a selector or locator on the current page or a selected frame. Built-in auto-wait ensures the element exists, is visible, has non-zero size, and is not disabled before clicking. If the click triggers a page navigation, waits for the new page to load before returning. Set auto_wait=false to disable the built-in wait.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the running profile"
            },
            "selector": {
              "type": "string",
              "description": "Legacy CSS selector for the element to click. Mutually exclusive with locator."
            },
            "locator": Self::locator_input_schema("Preferred locator definition for the element to click. Mutually exclusive with selector."),
            "frame": Self::frame_ref_input_schema("Optional frame selector. If provided, click runs inside that frame."),
            "auto_wait": {
              "type": "boolean",
              "description": "Whether to use built-in auto-wait before clicking. Default is true."
            },
            "timeout_ms": {
              "type": "integer",
              "description": "Maximum time for built-in auto-wait before clicking. Default is 10000."
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "type_text".to_string(),
        description: "Focus an element by selector or locator on the current page or a selected frame and type text into it. Built-in auto-wait ensures the target exists, is visible, remains editable, and can be focused before typing. By default uses realistic human-like typing with variable speed, natural errors, and self-corrections. Only set instant=true when you are certain the target does not have bot detection (e.g. browser address bars, developer tools, internal apps) — using instant on public websites risks the profile being flagged as a bot. Set auto_wait=false to disable the built-in wait.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the running profile"
            },
            "selector": {
              "type": "string",
              "description": "Legacy CSS selector for the input element. Mutually exclusive with locator."
            },
            "locator": Self::locator_input_schema("Preferred locator definition for the input element. Mutually exclusive with selector."),
            "frame": Self::frame_ref_input_schema("Optional frame selector. If provided, typing runs inside that frame."),
            "text": {
              "type": "string",
              "description": "Text to type into the element"
            },
            "auto_wait": {
              "type": "boolean",
              "description": "Whether to use built-in auto-wait before focusing and typing. Default is true."
            },
            "clear_first": {
              "type": "boolean",
              "description": "Clear the input before typing (default: true)"
            },
            "instant": {
              "type": "boolean",
              "description": "Paste all text at once instead of human typing. WARNING: only use on targets without bot detection — using this on public websites risks the profile being flagged."
            },
            "wpm": {
              "type": "number",
              "description": "Target words per minute for human typing (default: 80)"
            },
            "timeout_ms": {
              "type": "integer",
              "description": "Maximum time for built-in auto-wait before focusing and typing. Default is 10000."
            }
          },
          "required": ["profile_id", "text"],
          "description": "Server-side validation requires exactly one of selector or locator."
        }),
      },
      McpTool {
        name: "hover_element".to_string(),
        description: "Hover an element identified by a selector or locator on the current page or a selected frame.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string", "description": "The UUID of the running profile" },
            "selector": { "type": "string", "description": "Legacy CSS selector. Mutually exclusive with locator." },
            "locator": Self::locator_input_schema("Preferred locator definition. Mutually exclusive with selector."),
            "frame": Self::frame_ref_input_schema("Optional frame selector. If provided, hover runs inside that frame."),
            "auto_wait": { "type": "boolean", "description": "Whether to use built-in auto-wait before hovering. Default is true." },
            "timeout_ms": { "type": "integer", "description": "Maximum time for built-in auto-wait before hovering. Default is 10000." }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "focus_element".to_string(),
        description: "Focus an element identified by a selector or locator on the current page or a selected frame.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string", "description": "The UUID of the running profile" },
            "selector": { "type": "string", "description": "Legacy CSS selector. Mutually exclusive with locator." },
            "locator": Self::locator_input_schema("Preferred locator definition. Mutually exclusive with selector."),
            "frame": Self::frame_ref_input_schema("Optional frame selector. If provided, focus runs inside that frame."),
            "auto_wait": { "type": "boolean", "description": "Whether to use built-in auto-wait before focusing. Default is true." },
            "timeout_ms": { "type": "integer", "description": "Maximum time for built-in auto-wait before focusing. Default is 10000." }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "scroll_to".to_string(),
        description: "Scroll the current page or a selected frame to the given coordinates.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string", "description": "The UUID of the running profile" },
            "x": { "type": "number", "description": "Horizontal scroll position. Default is current x." },
            "y": { "type": "number", "description": "Vertical scroll position. Default is current y." },
            "behavior": { "type": "string", "enum": ["auto", "smooth", "instant"], "description": "Scroll behavior. Default is auto." },
            "frame": Self::frame_ref_input_schema("Optional frame selector. If provided, scrolling runs inside that frame.")
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "scroll_element_into_view".to_string(),
        description: "Scroll an element into view by selector or locator on the current page or a selected frame.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string", "description": "The UUID of the running profile" },
            "selector": { "type": "string", "description": "Legacy CSS selector. Mutually exclusive with locator." },
            "locator": Self::locator_input_schema("Preferred locator definition. Mutually exclusive with selector."),
            "frame": Self::frame_ref_input_schema("Optional frame selector. If provided, scrolling runs inside that frame."),
            "auto_wait": { "type": "boolean", "description": "Whether to use built-in auto-wait before scrolling. Default is true." },
            "timeout_ms": { "type": "integer", "description": "Maximum time for built-in auto-wait before scrolling. Default is 10000." }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "press_key".to_string(),
        description: "Press a single key on the current page or inside a selected frame.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string", "description": "The UUID of the running profile" },
            "key": { "type": "string", "description": "Key value to press, for example Enter or a." },
            "code": { "type": "string", "description": "Optional physical key code, for example KeyA or Enter." },
            "text": { "type": "string", "description": "Optional text payload for printable keys." },
            "frame": Self::frame_ref_input_schema("Optional frame selector. If provided, key press focuses that frame first.")
          },
          "required": ["profile_id", "key"]
        }),
      },
      McpTool {
        name: "press_hotkey".to_string(),
        description: "Press a hotkey chord such as Control+L or Meta+Shift+P on the current page or inside a selected frame.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string", "description": "The UUID of the running profile" },
            "keys": { "type": "array", "items": { "type": "string" }, "description": "Ordered key chord, for example [\"Control\", \"L\"]." },
            "frame": Self::frame_ref_input_schema("Optional frame selector. If provided, hotkey focuses that frame first.")
          },
          "required": ["profile_id", "keys"]
        }),
      },
      McpTool {
        name: "select_option".to_string(),
        description: "Select an option in a <select> element by value, label, or index on the current page or a selected frame.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string", "description": "The UUID of the running profile" },
            "selector": { "type": "string", "description": "Legacy CSS selector. Mutually exclusive with locator." },
            "locator": Self::locator_input_schema("Preferred locator definition. Mutually exclusive with selector."),
            "frame": Self::frame_ref_input_schema("Optional frame selector. If provided, selection runs inside that frame."),
            "value": { "type": "string", "description": "Select by option value." },
            "label": { "type": "string", "description": "Select by option label text." },
            "index": { "type": "integer", "description": "Select by zero-based option index." },
            "auto_wait": { "type": "boolean", "description": "Whether to use built-in auto-wait before selecting. Default is true." },
            "timeout_ms": { "type": "integer", "description": "Maximum time for built-in auto-wait before selecting. Default is 10000." }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "check_checkbox".to_string(),
        description: "Ensure a checkbox or radio-like control is checked on the current page or a selected frame.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string", "description": "The UUID of the running profile" },
            "selector": { "type": "string", "description": "Legacy CSS selector. Mutually exclusive with locator." },
            "locator": Self::locator_input_schema("Preferred locator definition. Mutually exclusive with selector."),
            "frame": Self::frame_ref_input_schema("Optional frame selector. If provided, checking runs inside that frame."),
            "auto_wait": { "type": "boolean", "description": "Whether to use built-in auto-wait before checking. Default is true." },
            "timeout_ms": { "type": "integer", "description": "Maximum time for built-in auto-wait before checking. Default is 10000." }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "uncheck_checkbox".to_string(),
        description: "Ensure a checkbox is unchecked on the current page or a selected frame.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string", "description": "The UUID of the running profile" },
            "selector": { "type": "string", "description": "Legacy CSS selector. Mutually exclusive with locator." },
            "locator": Self::locator_input_schema("Preferred locator definition. Mutually exclusive with selector."),
            "frame": Self::frame_ref_input_schema("Optional frame selector. If provided, unchecking runs inside that frame."),
            "auto_wait": { "type": "boolean", "description": "Whether to use built-in auto-wait before unchecking. Default is true." },
            "timeout_ms": { "type": "integer", "description": "Maximum time for built-in auto-wait before unchecking. Default is 10000." }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "element_exists".to_string(),
        description: "Check whether a locator resolves to an element on the current page of a running browser profile. Includes built-in auto-wait and returns the last observed state if the element never appears before timeout. Set auto_wait=false to read once without waiting.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the running profile"
            },
            "locator": Self::locator_input_schema("Locator definition to check for existence."),
            "frame": Self::frame_ref_input_schema("Optional frame selector. If provided, existence checking runs inside that frame."),
            "auto_wait": {
              "type": "boolean",
              "description": "Whether to use built-in auto-wait before returning. Default is true."
            },
            "timeout_ms": {
              "type": "integer",
              "description": "Maximum time for built-in auto-wait before returning. Default is 10000."
            }
          },
          "required": ["profile_id", "locator"]
        }),
      },
      McpTool {
        name: "get_element_text".to_string(),
        description: "Read the visible text of the element selected by a locator on the current page of a running browser profile. Includes built-in auto-wait until the element appears. Set auto_wait=false to read once without waiting.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the running profile"
            },
            "locator": Self::locator_input_schema("Locator definition for the element whose text should be returned."),
            "frame": Self::frame_ref_input_schema("Optional frame selector. If provided, text is read inside that frame."),
            "auto_wait": {
              "type": "boolean",
              "description": "Whether to use built-in auto-wait before returning text. Default is true."
            },
            "timeout_ms": {
              "type": "integer",
              "description": "Maximum time for built-in auto-wait before returning text. Default is 10000."
            }
          },
          "required": ["profile_id", "locator"]
        }),
      },
      McpTool {
        name: "get_element_html".to_string(),
        description: "Read the outerHTML of the element selected by a locator on the current page or a selected frame of a running browser profile. Includes built-in auto-wait until the element appears.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string", "description": "The UUID of the running profile" },
            "locator": Self::locator_input_schema("Locator definition for the element whose outerHTML should be returned."),
            "frame": Self::frame_ref_input_schema("Optional frame selector. If provided, reading runs inside that frame."),
            "auto_wait": { "type": "boolean", "description": "Whether to use built-in auto-wait before returning HTML. Default is true." },
            "timeout_ms": { "type": "integer", "description": "Maximum time for built-in auto-wait before returning HTML. Default is 10000." }
          },
          "required": ["profile_id", "locator"]
        }),
      },
      McpTool {
        name: "get_element_attribute".to_string(),
        description: "Read one attribute from the element selected by a locator on the current page or a selected frame of a running browser profile. Includes built-in auto-wait until the element appears.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string", "description": "The UUID of the running profile" },
            "locator": Self::locator_input_schema("Locator definition for the target element."),
            "frame": Self::frame_ref_input_schema("Optional frame selector. If provided, reading runs inside that frame."),
            "name": { "type": "string", "description": "Attribute name to read, for example href or aria-label." },
            "auto_wait": { "type": "boolean", "description": "Whether to use built-in auto-wait before returning the attribute. Default is true." },
            "timeout_ms": { "type": "integer", "description": "Maximum time for built-in auto-wait before returning the attribute. Default is 10000." }
          },
          "required": ["profile_id", "locator", "name"]
        }),
      },
      McpTool {
        name: "get_element_rect".to_string(),
        description: "Read the bounding client rect of the element selected by a locator on the current page or a selected frame of a running browser profile. Includes built-in auto-wait until the element appears.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string", "description": "The UUID of the running profile" },
            "locator": Self::locator_input_schema("Locator definition for the element whose rect should be returned."),
            "frame": Self::frame_ref_input_schema("Optional frame selector. If provided, reading runs inside that frame."),
            "auto_wait": { "type": "boolean", "description": "Whether to use built-in auto-wait before returning the rect. Default is true." },
            "timeout_ms": { "type": "integer", "description": "Maximum time for built-in auto-wait before returning the rect. Default is 10000." }
          },
          "required": ["profile_id", "locator"]
        }),
      },
      McpTool {
        name: "query_elements".to_string(),
        description: "Query elements by locator on the current page of a running browser profile and return structured summaries for the first matches. Includes built-in auto-wait and returns the last observed empty result if no match appears before timeout. Set auto_wait=false to read once without waiting.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the running profile"
            },
            "locator": Self::locator_input_schema("Locator definition for the elements to enumerate."),
            "frame": Self::frame_ref_input_schema("Optional frame selector. If provided, querying runs inside that frame."),
            "limit": {
              "type": "integer",
              "description": "Maximum number of element summaries to return. Default is 20, max is 100."
            },
            "auto_wait": {
              "type": "boolean",
              "description": "Whether to use built-in auto-wait before returning results. Default is true."
            },
            "timeout_ms": {
              "type": "integer",
              "description": "Maximum time for built-in auto-wait before returning. Default is 10000."
            }
          },
          "required": ["profile_id", "locator"]
        }),
      },
      McpTool {
        name: "count_elements".to_string(),
        description: "Count how many elements currently match a locator on the current page of a running browser profile.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the running profile"
            },
            "locator": Self::locator_input_schema("Locator definition for the elements to count.")
          },
          "required": ["profile_id", "locator"]
        }),
      },
      McpTool {
        name: "get_page_content".to_string(),
        description:
          "Get the content of the current page or a selected frame. Works with both static HTML and JavaScript-rendered content."
            .to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the running profile"
            },
            "format": {
              "type": "string",
              "enum": ["html", "text"],
              "description": "Content format: 'html' for full HTML, 'text' for visible text only (default: text)"
            },
            "selector": {
              "type": "string",
              "description": "Optional CSS selector to get content of a specific element instead of the whole page"
            },
            "frame": Self::frame_ref_input_schema("Optional frame selector. If provided, content is read from that frame.")
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "get_page_info".to_string(),
        description: "Get metadata about the current page including URL, title, and readiness state"
          .to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the running profile"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "list_tabs".to_string(),
        description: "List all open page tabs for a running browser profile. The active tab is the one MCP browser automation currently targets.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the running profile"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "switch_tab".to_string(),
        description: "Switch MCP browser automation to a different open tab by id, index, URL substring, or title substring.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the running profile"
            },
            "tab": Self::tab_ref_input_schema("Tab selector. Supports id, index, url, and title.")
          },
          "required": ["profile_id", "tab"]
        }),
      },
      McpTool {
        name: "new_tab".to_string(),
        description: "Open a new tab for a running browser profile and make it the active MCP browser automation tab.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the running profile"
            },
            "url": {
              "type": "string",
              "description": "Optional URL to open in the new tab. Defaults to about:blank."
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "close_tab".to_string(),
        description: "Close a tab in a running browser profile. If tab is omitted, closes the current active MCP browser automation tab.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the running profile"
            },
            "tab": Self::tab_ref_input_schema("Optional tab selector. Supports id, index, url, and title.")
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "get_active_tab".to_string(),
        description: "Get the active tab that MCP browser automation currently targets for a running browser profile.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the running profile"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "set_file_input".to_string(),
        description: "Set one or more local files on a file input element in the current page or a selected frame.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string", "description": "The UUID of the running profile" },
            "selector": { "type": "string", "description": "Legacy CSS selector. Mutually exclusive with locator." },
            "locator": Self::locator_input_schema("Preferred locator definition. Mutually exclusive with selector."),
            "frame": Self::frame_ref_input_schema("Optional frame selector. If provided, file selection runs inside that frame."),
            "files": {
              "type": "array",
              "items": { "type": "string" },
              "description": "Absolute or relative local file paths to attach to the input."
            },
            "auto_wait": { "type": "boolean", "description": "Whether to wait for the file input to become visible and enabled. Default is true." },
            "timeout_ms": { "type": "integer", "description": "Maximum time for built-in auto-wait before setting files. Default is 10000." }
          },
          "required": ["profile_id", "files"]
        }),
      },
      McpTool {
        name: "upload_file".to_string(),
        description: "Convenience wrapper around set_file_input for file-upload automation.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string", "description": "The UUID of the running profile" },
            "selector": { "type": "string", "description": "Legacy CSS selector. Mutually exclusive with locator." },
            "locator": Self::locator_input_schema("Preferred locator definition. Mutually exclusive with selector."),
            "frame": Self::frame_ref_input_schema("Optional frame selector. If provided, upload runs inside that frame."),
            "files": {
              "type": "array",
              "items": { "type": "string" },
              "description": "Absolute or relative local file paths to attach to the input."
            },
            "auto_wait": { "type": "boolean", "description": "Whether to wait for the file input to become visible and enabled. Default is true." },
            "timeout_ms": { "type": "integer", "description": "Maximum time for built-in auto-wait before uploading. Default is 10000." }
          },
          "required": ["profile_id", "files"]
        }),
      },
      McpTool {
        name: "download_file".to_string(),
        description: "Download a file using the running profile's current session cookies into that profile's dedicated mcp-downloads directory.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string", "description": "The UUID of the running profile" },
            "url": { "type": "string", "description": "Optional URL to download. Defaults to the active tab URL." },
            "file_name": { "type": "string", "description": "Optional file name override for the saved artifact." }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "wait_for_download".to_string(),
        description: "Wait for a previously-started download_file operation to finish.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": { "type": "string", "description": "The UUID of the running profile" },
            "download_id": { "type": "string", "description": "Download identifier returned by download_file." },
            "timeout_ms": { "type": "integer", "description": "Maximum time to wait before timing out. Default is 10000." }
          },
          "required": ["profile_id", "download_id"]
        }),
      },
      McpTool {
        name: "list_frames".to_string(),
        description: "List all frames on the current page of a running browser profile as a flattened preorder sequence. Main frame is always index 0.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the running profile"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "get_frame_tree".to_string(),
        description: "Return the nested frame tree for the current page of a running browser profile, including child iframes.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the running profile"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "find_frame".to_string(),
        description: "Resolve a single frame on the current page of a running browser profile by main, index, id, name, or url.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the running profile"
            },
            "frame": Self::frame_ref_input_schema("Frame selector. Supports main, index, id, name, and url.")
          },
          "required": ["profile_id", "frame"]
        }),
      },
      McpTool {
        name: "start_console_capture".to_string(),
        description: "Start background console log capture for a running browser profile. Safe to call again if capture is already active.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the running profile"
            },
            "timeout_ms": {
              "type": "integer",
              "description": "Optional startup timeout for establishing the console capture connection"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "get_console_logs".to_string(),
        description: "Read captured console logs for a browser profile.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile"
            },
            "limit": {
              "type": "integer",
              "description": "Optional maximum number of most recent entries to return"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "clear_console_logs".to_string(),
        description: "Clear buffered console logs for a browser profile.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "start_network_capture".to_string(),
        description: "Start background network request capture for a running browser profile. Safe to call again if capture is already active.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the running profile"
            },
            "timeout_ms": {
              "type": "integer",
              "description": "Optional startup timeout for establishing the network capture connection"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "get_network_requests".to_string(),
        description: "Read captured network requests for a browser profile.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile"
            },
            "limit": {
              "type": "integer",
              "description": "Optional maximum number of most recent requests to return"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "get_response_body".to_string(),
        description: "Fetch the response body for a previously captured request ID.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile"
            },
            "request_id": {
              "type": "string",
              "description": "The CDP requestId returned by get_network_requests"
            },
            "max_bytes": {
              "type": "integer",
              "description": "Maximum response body size to return before failing (default: 262144)"
            }
          },
          "required": ["profile_id", "request_id"]
        }),
      },
      McpTool {
        name: "clear_network_requests".to_string(),
        description: "Clear buffered network requests for a browser profile.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "get_all_traffic_snapshots".to_string(),
        description: "Get lightweight traffic snapshots for all profiles with traffic statistics.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "get_traffic_stats_for_period".to_string(),
        description: "Get detailed traffic statistics for a profile over a recent time window.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile"
            },
            "seconds": {
              "type": "integer",
              "description": "Time window in seconds. Use 0 for all-time statistics."
            }
          },
          "required": ["profile_id", "seconds"]
        }),
      },
      McpTool {
        name: "clear_all_traffic_stats".to_string(),
        description: "Clear all stored traffic statistics snapshots.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "read_profile_cookies".to_string(),
        description: "Read and group cookies from a browser profile.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "The UUID of the profile"
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "copy_profile_cookies".to_string(),
        description: "Copy cookies from one profile to one or more target profiles.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "source_profile_id": {
              "type": "string",
              "description": "Source profile UUID"
            },
            "target_profile_ids": {
              "type": "array",
              "items": { "type": "string" },
              "description": "Target profile UUID list"
            },
            "selected_cookies": {
              "type": "array",
              "description": "Optional subset to copy. If omitted or empty, all cookies are copied.",
              "items": {
                "type": "object",
                "properties": {
                  "domain": {
                    "type": "string",
                    "description": "Cookie domain"
                  },
                  "name": {
                    "type": "string",
                    "description": "Cookie name. Leave empty to copy all cookies for the domain."
                  }
                },
                "required": ["domain"]
              }
            }
          },
          "required": ["source_profile_id", "target_profile_ids"]
        }),
      },
      McpTool {
        name: "import_cookies_from_file".to_string(),
        description: "Import cookies into a profile from raw JSON or Netscape cookie content.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "Target profile UUID"
            },
            "content": {
              "type": "string",
              "description": "Cookie file content in JSON or Netscape format"
            }
          },
          "required": ["profile_id", "content"]
        }),
      },
      McpTool {
        name: "export_profile_cookies".to_string(),
        description: "Export cookies from a profile as JSON or Netscape format.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "Profile UUID"
            },
            "format": {
              "type": "string",
              "enum": ["json", "netscape"],
              "description": "Export format"
            }
          },
          "required": ["profile_id", "format"]
        }),
      },
      McpTool {
        name: "get_local_storage".to_string(),
        description: "Read localStorage from the current page of a running browser profile.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "Running profile UUID"
            },
            "key": {
              "type": "string",
              "description": "Optional storage key. If omitted, all localStorage entries are returned."
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "set_local_storage".to_string(),
        description: "Set one localStorage key on the current page of a running browser profile.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "Running profile UUID"
            },
            "key": {
              "type": "string",
              "description": "Storage key"
            },
            "value": {
              "description": "Value to store. Strings are stored verbatim; other JSON types are stored as JSON strings."
            }
          },
          "required": ["profile_id", "key", "value"]
        }),
      },
      McpTool {
        name: "get_session_storage".to_string(),
        description: "Read sessionStorage from the current page of a running browser profile.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "Running profile UUID"
            },
            "key": {
              "type": "string",
              "description": "Optional storage key. If omitted, all sessionStorage entries are returned."
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "clear_storage".to_string(),
        description: "Clear or delete local/session storage on the current page of a running browser profile.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "Running profile UUID"
            },
            "storage": {
              "type": "string",
              "enum": ["local", "session", "both"],
              "description": "Storage area to clear. Default is both."
            },
            "key": {
              "type": "string",
              "description": "Optional single key to remove instead of clearing the whole storage area."
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "wait_for_selector".to_string(),
        description: "Wait until a locator exists or becomes visible on the current page or inside a selected frame of a running browser profile.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "Running profile UUID"
            },
            "locator": Self::locator_input_schema("Locator definition. Supports css, text, testid, and xpath."),
            "frame": Self::frame_ref_input_schema("Optional frame selector. If provided, waiting runs inside that frame."),
            "state": {
              "type": "string",
              "enum": ["exists", "visible"],
              "description": "Wait condition. Default is visible."
            },
            "timeout_ms": {
              "type": "integer",
              "description": "Timeout in milliseconds. Default is 10000."
            }
          },
          "required": ["profile_id", "locator"]
        }),
      },
      McpTool {
        name: "wait_for_text".to_string(),
        description: "Wait until page text or text inside a selector/locator scope appears on the current page of a running browser profile.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "Running profile UUID"
            },
            "text": {
              "type": "string",
              "description": "Text content to wait for"
            },
            "selector": {
              "type": "string",
              "description": "Optional CSS selector scope. Mutually exclusive with locator."
            },
            "locator": Self::locator_input_schema("Optional locator scope. Mutually exclusive with selector."),
            "exact": {
              "type": "boolean",
              "description": "Require exact text equality instead of contains. Default is false."
            },
            "timeout_ms": {
              "type": "integer",
              "description": "Timeout in milliseconds. Default is 10000."
            }
          },
          "required": ["profile_id", "text"]
        }),
      },
      McpTool {
        name: "wait_for_navigation".to_string(),
        description: "Wait until the current page finishes navigating or reloading in a running browser profile.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "profile_id": {
              "type": "string",
              "description": "Running profile UUID"
            },
            "from_url": {
              "type": "string",
              "description": "Optional baseline URL. If provided, wait until the current URL differs from it and the page is complete."
            },
            "timeout_ms": {
              "type": "integer",
              "description": "Timeout in milliseconds. Default is 10000."
            }
          },
          "required": ["profile_id"]
        }),
      },
      McpTool {
        name: "generate_sample_fingerprint".to_string(),
        description: "Generate a sample fingerprint JSON string for a browser/version/config combination without mutating a profile.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "browser": {
              "type": "string",
              "enum": ["chromium", "camoufox"],
              "description": "Browser engine"
            },
            "version": {
              "type": "string",
              "description": "Browser version to target"
            },
            "config": {
              "type": "object",
              "description": "Browser-specific config object used for fingerprint generation"
            }
          },
          "required": ["browser", "version", "config"]
        }),
      },
      McpTool {
        name: "is_geoip_database_available".to_string(),
        description: "Check whether the GeoIP database required by Camoufox geolocation features is already available.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
      McpTool {
        name: "download_geoip_database".to_string(),
        description: "Download or refresh the GeoIP database used by Camoufox geolocation features.".to_string(),
        input_schema: serde_json::json!({
          "type": "object",
          "properties": {},
          "required": []
        }),
      },
    ]
  }

  async fn handle_initialize(
    &self,
    request: McpRequest,
  ) -> Result<(String, (serde_json::Value, serde_json::Value)), (serde_json::Value, McpError)> {
    let id = request.id.clone().unwrap_or(serde_json::Value::Null);

    if !self.is_running() {
      return Err((id, McpError::not_running()));
    }

    // Create session
    let session_id = Uuid::new_v4().to_string();
    {
      let mut inner = self.inner.lock().await;
      inner
        .sessions
        .insert(session_id.clone(), McpSession { initialized: false });
    }

    let result = serde_json::json!({
      "protocolVersion": PROTOCOL_VERSION,
      "capabilities": {
        "tools": {
          "listChanged": false
        }
      },
      "serverInfo": {
        "name": SERVER_NAME,
        "version": SERVER_VERSION,
      },
      "instructions": "JnmBrowser MCP server. Use tools/list to discover available browser automation tools."
    });

    log::info!("[mcp] New session initialized: {}", session_id);
    Ok((session_id, (id, result)))
  }

  pub async fn handle_request(&self, request: McpRequest) -> McpResponse {
    let id = request.id.clone().unwrap_or(serde_json::Value::Null);

    if !self.is_running() {
      return McpResponse::failure(id, McpError::not_running());
    }

    let result = match request.method.as_str() {
      "ping" => Ok(serde_json::json!({})),
      "tools/list" => self.handle_tools_list().await,
      "tools/call" => self.handle_tool_call(request.params).await,
      _ => Err(McpError::method_not_found(format!(
        "Method not found: {}",
        request.method
      ))),
    };

    match result {
      Ok(value) => McpResponse::success(id, value),
      Err(error) => McpResponse::failure(id, error),
    }
  }

  async fn handle_tools_list(&self) -> Result<serde_json::Value, McpError> {
    Ok(serde_json::json!({
      "tools": self.get_tools()
    }))
  }

  async fn handle_tool_call(
    &self,
    params: Option<serde_json::Value>,
  ) -> Result<serde_json::Value, McpError> {
    let params = params.ok_or_else(|| McpError::invalid_params("Missing parameters"))?;

    let tool_name = params
      .get("name")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError::invalid_params("Missing tool name"))?;

    let arguments = params
      .get("arguments")
      .cloned()
      .unwrap_or(serde_json::json!({}));

    match tool_name {
      "list_profiles" => self.handle_list_profiles().await,
      "get_profile" => self.handle_get_profile(&arguments).await,
      "run_profile" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_run_profile(&arguments).await
      }
      "kill_profile" => self.handle_kill_profile(&arguments).await,
      "create_profile" => self.handle_create_profile(&arguments).await,
      "update_profile" => self.handle_update_profile(&arguments).await,
      "delete_profile" => self.handle_delete_profile(&arguments).await,
      "list_tags" => self.handle_list_tags().await,
      "list_proxies" => self.handle_list_proxies().await,
      "get_profile_status" => self.handle_get_profile_status(&arguments).await,
      // Group management
      "list_groups" => self.handle_list_groups().await,
      "get_group" => self.handle_get_group(&arguments).await,
      "create_group" => self.handle_create_group(&arguments).await,
      "update_group" => self.handle_update_group(&arguments).await,
      "delete_group" => self.handle_delete_group(&arguments).await,
      "assign_profiles_to_group" => self.handle_assign_profiles_to_group(&arguments).await,
      // Full proxy management
      "get_proxy" => self.handle_get_proxy(&arguments).await,
      "create_proxy" => self.handle_create_proxy(&arguments).await,
      "update_proxy" => self.handle_update_proxy(&arguments).await,
      "delete_proxy" => self.handle_delete_proxy(&arguments).await,
      // Proxy import/export
      "export_proxies" => self.handle_export_proxies(&arguments).await,
      "import_proxies" => self.handle_import_proxies(&arguments).await,
      // VPN management
      "import_vpn" => self.handle_import_vpn(&arguments).await,
      "list_vpn_configs" => self.handle_list_vpn_configs().await,
      "delete_vpn" => self.handle_delete_vpn(&arguments).await,
      "connect_vpn" => self.handle_connect_vpn(&arguments).await,
      "disconnect_vpn" => self.handle_disconnect_vpn(&arguments).await,
      "get_vpn_status" => self.handle_get_vpn_status(&arguments).await,
      // Fingerprint management
      "get_profile_fingerprint" => self.handle_get_profile_fingerprint(&arguments).await,
      "update_profile_fingerprint" => self.handle_update_profile_fingerprint(&arguments).await,
      "update_profile_proxy_bypass_rules" => {
        self
          .handle_update_profile_proxy_bypass_rules(&arguments)
          .await
      }
      // DNS blocklist management
      "update_profile_dns_blocklist" => self.handle_update_profile_dns_blocklist(&arguments).await,
      "get_dns_blocklist_status" => self.handle_get_dns_blocklist_status().await,
      // Extension management
      "list_extensions" => self.handle_list_extensions().await,
      "list_extension_groups" => self.handle_list_extension_groups().await,
      "create_extension_group" => self.handle_create_extension_group(&arguments).await,
      "delete_extension" => self.handle_delete_extension_mcp(&arguments).await,
      "delete_extension_group" => self.handle_delete_extension_group_mcp(&arguments).await,
      "assign_extension_group_to_profile" => {
        self
          .handle_assign_extension_group_to_profile(&arguments)
          .await
      }
      // Team lock tools
      "get_team_locks" => self.handle_get_team_locks().await,
      "get_team_lock_status" => self.handle_get_team_lock_status(&arguments).await,
      "get_sync_settings" => self.handle_get_sync_settings().await,
      "save_sync_settings" => self.handle_save_sync_settings(&arguments).await,
      "request_profile_sync" => self.handle_request_profile_sync(&arguments).await,
      "validate_recipe" => self.handle_validate_recipe(&arguments).await,
      "preview_recipe" => self.handle_preview_recipe(&arguments).await,
      "run_recipe" => self.handle_run_recipe(&arguments).await,
      "run_batch_profile_workflow" => self.handle_run_batch_profile_workflow(&arguments).await,
      // Synchronizer tools
      "start_sync_session" => self.handle_start_sync_session(&arguments).await,
      "stop_sync_session" => self.handle_stop_sync_session(&arguments).await,
      "get_sync_sessions" => self.handle_get_sync_sessions().await,
      "remove_sync_follower" => self.handle_remove_sync_follower(&arguments).await,
      // Action recorder tools
      "start_action_recording" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_start_action_recording(&arguments).await
      }
      "stop_action_recording" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_stop_action_recording(&arguments).await
      }
      "get_recorded_events" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_get_recorded_events().await
      }
      "list_recordings" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_list_recordings().await
      }
      "export_recording_as_recipe" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_export_recording_as_recipe(&arguments).await
      }
      "replay_recording" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_replay_recording(&arguments).await
      }
      // Browser interaction tools (require paid subscription)
      "navigate" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_navigate(&arguments).await
      }
      "screenshot" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_screenshot(&arguments).await
      }
      "evaluate_javascript" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_evaluate_javascript(&arguments).await
      }
      "click_element" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_click_element(&arguments).await
      }
      "type_text" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_type_text(&arguments).await
      }
      "hover_element" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_hover_element(&arguments).await
      }
      "focus_element" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_focus_element(&arguments).await
      }
      "scroll_to" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_scroll_to(&arguments).await
      }
      "scroll_element_into_view" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_scroll_element_into_view(&arguments).await
      }
      "press_key" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_press_key(&arguments).await
      }
      "press_hotkey" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_press_hotkey(&arguments).await
      }
      "select_option" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_select_option(&arguments).await
      }
      "check_checkbox" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_check_checkbox(&arguments).await
      }
      "uncheck_checkbox" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_uncheck_checkbox(&arguments).await
      }
      "element_exists" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_element_exists(&arguments).await
      }
      "get_element_text" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_get_element_text(&arguments).await
      }
      "get_element_html" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_get_element_html(&arguments).await
      }
      "get_element_attribute" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_get_element_attribute(&arguments).await
      }
      "get_element_rect" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_get_element_rect(&arguments).await
      }
      "query_elements" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_query_elements(&arguments).await
      }
      "count_elements" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_count_elements(&arguments).await
      }
      "get_page_content" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_get_page_content(&arguments).await
      }
      "get_page_info" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_get_page_info(&arguments).await
      }
      "list_tabs" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_list_tabs(&arguments).await
      }
      "switch_tab" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_switch_tab(&arguments).await
      }
      "new_tab" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_new_tab(&arguments).await
      }
      "close_tab" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_close_tab(&arguments).await
      }
      "get_active_tab" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_get_active_tab(&arguments).await
      }
      "set_file_input" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_set_file_input(&arguments).await
      }
      "upload_file" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_upload_file(&arguments).await
      }
      "download_file" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_download_file(&arguments).await
      }
      "wait_for_download" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_wait_for_download(&arguments).await
      }
      "list_frames" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_list_frames(&arguments).await
      }
      "get_frame_tree" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_get_frame_tree(&arguments).await
      }
      "find_frame" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_find_frame(&arguments).await
      }
      "start_console_capture" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_start_console_capture(&arguments).await
      }
      "get_console_logs" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_get_console_logs(&arguments).await
      }
      "clear_console_logs" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_clear_console_logs(&arguments).await
      }
      "start_network_capture" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_start_network_capture(&arguments).await
      }
      "get_network_requests" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_get_network_requests(&arguments).await
      }
      "get_response_body" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_get_response_body(&arguments).await
      }
      "clear_network_requests" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_clear_network_requests(&arguments).await
      }
      "get_all_traffic_snapshots" => self.handle_get_all_traffic_snapshots().await,
      "get_traffic_stats_for_period" => self.handle_get_traffic_stats_for_period(&arguments).await,
      "clear_all_traffic_stats" => self.handle_clear_all_traffic_stats().await,
      "read_profile_cookies" => self.handle_read_profile_cookies(&arguments).await,
      "copy_profile_cookies" => self.handle_copy_profile_cookies(&arguments).await,
      "import_cookies_from_file" => self.handle_import_cookies_from_file(&arguments).await,
      "export_profile_cookies" => self.handle_export_profile_cookies(&arguments).await,
      "get_local_storage" => self.handle_get_local_storage(&arguments).await,
      "set_local_storage" => self.handle_set_local_storage(&arguments).await,
      "get_session_storage" => self.handle_get_session_storage(&arguments).await,
      "clear_storage" => self.handle_clear_storage(&arguments).await,
      "wait_for_selector" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_wait_for_selector(&arguments).await
      }
      "wait_for_text" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_wait_for_text(&arguments).await
      }
      "wait_for_navigation" => {
        Self::require_paid_subscription("Browser automation").await?;
        self.handle_wait_for_navigation(&arguments).await
      }
      "generate_sample_fingerprint" => self.handle_generate_sample_fingerprint(&arguments).await,
      "is_geoip_database_available" => self.handle_is_geoip_database_available().await,
      "download_geoip_database" => self.handle_download_geoip_database().await,
      _ => Err(McpError::invalid_params(format!(
        "Unknown tool: {tool_name}"
      ))),
    }
  }

  async fn handle_list_profiles(&self) -> Result<serde_json::Value, McpError> {
    let profiles = ProfileManager::instance()
      .list_profiles()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list profiles: {e}"),
      })?;

    // Filter to only Chromium-compatible and Camoufox profiles
    let filtered: Vec<serde_json::Value> = profiles
      .iter()
      .filter(|p| is_supported_mcp_browser(&p.browser))
      .map(serialize_profile_for_mcp)
      .collect();

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&filtered).unwrap_or_default()
      }]
    }))
  }

  async fn handle_get_profile(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = arguments
      .get("profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing profile_id".to_string(),
      })?;

    let profiles = ProfileManager::instance()
      .list_profiles()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list profiles: {e}"),
      })?;

    let profile = profiles
      .iter()
      .find(|p| p.id.to_string() == profile_id)
      .ok_or_else(|| McpError {
        code: -32000,
        message: format!("Profile not found: {profile_id}"),
      })?;

    // Check if it's a Chromium or Camoufox profile
    if !is_supported_mcp_browser(&profile.browser) {
      return Err(McpError {
        code: -32000,
        message: "MCP only supports Chromium and Camoufox profiles".to_string(),
      });
    }

    let serialized_profile = serialize_profile_for_mcp(profile);

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&serialized_profile).unwrap_or_default()
      }]
    }))
  }

  async fn handle_run_profile(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = arguments
      .get("profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing profile_id".to_string(),
      })?;

    let url = arguments.get("url").and_then(|v| v.as_str());
    let _headless = arguments
      .get("headless")
      .and_then(|v| v.as_bool())
      .unwrap_or(false);

    // Get the profile
    let profiles = ProfileManager::instance()
      .list_profiles()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list profiles: {e}"),
      })?;

    let profile = profiles
      .iter()
      .find(|p| p.id.to_string() == profile_id)
      .ok_or_else(|| McpError {
        code: -32000,
        message: format!("Profile not found: {profile_id}"),
      })?;

    // Check if it's a Chromium or Camoufox profile
    if !is_supported_mcp_browser(&profile.browser) {
      return Err(McpError {
        code: -32000,
        message: "MCP only supports Chromium and Camoufox profiles".to_string(),
      });
    }

    // Team lock check
    crate::team_lock::acquire_team_lock_if_needed(profile)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;

    // Get app handle to launch
    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    // Launch the browser
    crate::browser_runner::BrowserRunner::instance()
      .launch_browser(
        app_handle.clone(),
        profile,
        url.map(|s| s.to_string()),
        None,
      )
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to launch browser: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Browser profile '{}' launched successfully", profile.name)
      }]
    }))
  }

  async fn handle_kill_profile(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = arguments
      .get("profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing profile_id".to_string(),
      })?;

    // Get the profile
    let profiles = ProfileManager::instance()
      .list_profiles()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list profiles: {e}"),
      })?;

    let profile = profiles
      .iter()
      .find(|p| p.id.to_string() == profile_id)
      .ok_or_else(|| McpError {
        code: -32000,
        message: format!("Profile not found: {profile_id}"),
      })?;

    // Check if it's a Chromium or Camoufox profile
    if !is_supported_mcp_browser(&profile.browser) {
      return Err(McpError {
        code: -32000,
        message: "MCP only supports Chromium and Camoufox profiles".to_string(),
      });
    }

    // Get app handle to kill
    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    // Kill the browser
    crate::browser_runner::BrowserRunner::instance()
      .kill_browser_process(app_handle.clone(), profile)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to kill browser: {e}"),
      })?;

    crate::team_lock::release_team_lock_if_needed(profile).await;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Browser profile '{}' stopped successfully", profile.name)
      }]
    }))
  }

  async fn handle_create_profile(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing name".to_string(),
      })?;
    let browser = arguments
      .get("browser")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing browser".to_string(),
      })?;

    let normalized_browser = crate::browser::normalize_browser_name(browser);

    if normalized_browser != "chromium" && normalized_browser != "camoufox" {
      return Err(McpError {
        code: -32602,
        message: "browser must be 'chromium' or 'camoufox'".to_string(),
      });
    }

    let proxy_id = arguments
      .get("proxy_id")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());
    let launch_hook = arguments
      .get("launch_hook")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());
    let group_id = arguments
      .get("group_id")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());
    let tags: Option<Vec<String>> = arguments.get("tags").and_then(|v| {
      v.as_array().map(|arr| {
        arr
          .iter()
          .filter_map(|item| item.as_str().map(|s| s.to_string()))
          .collect()
      })
    });

    // Pick the latest downloaded version for this browser
    let registry = crate::downloaded_browsers_registry::DownloadedBrowsersRegistry::instance();
    let versions = registry.get_downloaded_versions(normalized_browser);
    let version = versions.first().ok_or_else(|| McpError {
      code: -32000,
      message: format!("No downloaded version found for {normalized_browser}. Download it first."),
    })?;

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    let mut profile = ProfileManager::instance()
      .create_profile_with_group(
        app_handle,
        name,
        normalized_browser,
        version,
        "stable",
        proxy_id,
        None,
        None,
        None,
        group_id,
        false,
        None,
        launch_hook,
      )
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to create profile: {e}"),
      })?;

    if let Some(tags) = tags {
      let _ =
        ProfileManager::instance().update_profile_tags(app_handle, &profile.name, tags.clone());
      profile.tags = tags;
      if let Ok(profiles) = ProfileManager::instance().list_profiles() {
        let _ = crate::tag_manager::TAG_MANAGER
          .lock()
          .map(|manager| manager.rebuild_from_profiles(&profiles));
      }
    }

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Profile '{}' created (id: {})", profile.name, profile.id)
      }]
    }))
  }

  async fn handle_update_profile(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = arguments
      .get("profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing profile_id".to_string(),
      })?;

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;
    let pm = ProfileManager::instance();

    if let Some(new_name) = arguments.get("name").and_then(|v| v.as_str()) {
      pm.rename_profile(app_handle, profile_id, new_name)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to rename profile: {e}"),
        })?;
    }

    if let Some(proxy_id) = arguments.get("proxy_id").and_then(|v| v.as_str()) {
      let pid = if proxy_id.is_empty() {
        None
      } else {
        Some(proxy_id.to_string())
      };
      pm.update_profile_proxy(app_handle.clone(), profile_id, pid)
        .await
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update proxy: {e}"),
        })?;
    }

    if let Some(launch_hook) = arguments.get("launch_hook").and_then(|v| v.as_str()) {
      let normalized = if launch_hook.is_empty() {
        None
      } else {
        Some(launch_hook.to_string())
      };
      pm.update_profile_launch_hook(app_handle, profile_id, normalized)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update launch hook: {e}"),
        })?;
    }

    if let Some(group_id) = arguments.get("group_id").and_then(|v| v.as_str()) {
      let gid = if group_id.is_empty() {
        None
      } else {
        Some(group_id.to_string())
      };
      pm.assign_profiles_to_group(app_handle, vec![profile_id.to_string()], gid)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update group: {e}"),
        })?;
    }

    if let Some(tags) = arguments.get("tags").and_then(|v| v.as_array()) {
      let tag_list: Vec<String> = tags
        .iter()
        .filter_map(|item| item.as_str().map(|s| s.to_string()))
        .collect();
      pm.update_profile_tags(app_handle, profile_id, tag_list)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update tags: {e}"),
        })?;
      if let Ok(profiles) = pm.list_profiles() {
        let _ = crate::tag_manager::TAG_MANAGER
          .lock()
          .map(|manager| manager.rebuild_from_profiles(&profiles));
      }
    }

    if let Some(ext_group_id) = arguments.get("extension_group_id").and_then(|v| v.as_str()) {
      let eid = if ext_group_id.is_empty() {
        None
      } else {
        Some(ext_group_id.to_string())
      };
      pm.update_profile_extension_group(profile_id, eid)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update extension group: {e}"),
        })?;
    }

    if let Some(rules) = arguments
      .get("proxy_bypass_rules")
      .and_then(|v| v.as_array())
    {
      let rule_list: Vec<String> = rules
        .iter()
        .filter_map(|item| item.as_str().map(|s| s.to_string()))
        .collect();
      pm.update_profile_proxy_bypass_rules(app_handle, profile_id, rule_list)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to update proxy bypass rules: {e}"),
        })?;
    }

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Profile '{profile_id}' updated successfully")
      }]
    }))
  }

  async fn handle_delete_profile(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = arguments
      .get("profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing profile_id".to_string(),
      })?;

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    ProfileManager::instance()
      .delete_profile(app_handle, profile_id)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to delete profile: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Profile '{profile_id}' deleted successfully")
      }]
    }))
  }

  async fn handle_list_tags(&self) -> Result<serde_json::Value, McpError> {
    let tags = crate::tag_manager::TAG_MANAGER
      .lock()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to access tag manager: {e}"),
      })?
      .get_all_tags()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to get tags: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&tags).unwrap_or_default()
      }]
    }))
  }

  async fn handle_list_proxies(&self) -> Result<serde_json::Value, McpError> {
    let proxies = PROXY_MANAGER.get_stored_proxies();

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&proxies).unwrap_or_default()
      }]
    }))
  }

  async fn handle_get_profile_status(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = arguments
      .get("profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing profile_id".to_string(),
      })?;

    // Get the profile
    let profiles = ProfileManager::instance()
      .list_profiles()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list profiles: {e}"),
      })?;

    let profile = profiles
      .iter()
      .find(|p| p.id.to_string() == profile_id)
      .ok_or_else(|| McpError {
        code: -32000,
        message: format!("Profile not found: {profile_id}"),
      })?;

    // Check if it's a Chromium or Camoufox profile
    if !is_supported_mcp_browser(&profile.browser) {
      return Err(McpError {
        code: -32000,
        message: "MCP only supports Chromium and Camoufox profiles".to_string(),
      });
    }

    let is_running = profile.process_id.is_some();

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::json!({
          "profile_id": profile_id,
          "is_running": is_running
        }).to_string()
      }]
    }))
  }

  // Group management handlers
  async fn handle_list_groups(&self) -> Result<serde_json::Value, McpError> {
    let groups = GROUP_MANAGER
      .lock()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to lock group manager: {e}"),
      })?
      .get_all_groups()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list groups: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&groups).unwrap_or_default()
      }]
    }))
  }

  async fn handle_get_group(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let group_id = arguments
      .get("group_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing group_id".to_string(),
      })?;

    let groups = GROUP_MANAGER
      .lock()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to lock group manager: {e}"),
      })?
      .get_all_groups()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list groups: {e}"),
      })?;

    let group = groups
      .iter()
      .find(|g| g.id == group_id)
      .ok_or_else(|| McpError {
        code: -32000,
        message: format!("Group not found: {group_id}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&group).unwrap_or_default()
      }]
    }))
  }

  async fn handle_create_group(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing name".to_string(),
      })?;

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    let group = GROUP_MANAGER
      .lock()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to lock group manager: {e}"),
      })?
      .create_group(app_handle, name.to_string())
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to create group: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Group '{}' created successfully with ID: {}", group.name, group.id)
      }]
    }))
  }

  async fn handle_update_group(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let group_id = arguments
      .get("group_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing group_id".to_string(),
      })?;

    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing name".to_string(),
      })?;

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    let group = GROUP_MANAGER
      .lock()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to lock group manager: {e}"),
      })?
      .update_group(app_handle, group_id.to_string(), name.to_string())
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to update group: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Group '{}' updated successfully", group.name)
      }]
    }))
  }

  async fn handle_delete_group(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let group_id = arguments
      .get("group_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing group_id".to_string(),
      })?;

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    GROUP_MANAGER
      .lock()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to lock group manager: {e}"),
      })?
      .delete_group(app_handle, group_id.to_string())
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to delete group: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Group '{}' deleted successfully", group_id)
      }]
    }))
  }

  async fn handle_assign_profiles_to_group(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_ids: Vec<String> = arguments
      .get("profile_ids")
      .and_then(|v| v.as_array())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing profile_ids".to_string(),
      })?
      .iter()
      .filter_map(|v| v.as_str().map(|s| s.to_string()))
      .collect();

    let group_id = arguments
      .get("group_id")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    ProfileManager::instance()
      .assign_profiles_to_group(app_handle, profile_ids.clone(), group_id.clone())
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to assign profiles to group: {e}"),
      })?;

    let group_name = group_id.as_deref().unwrap_or("default");
    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("{} profile(s) assigned to group '{}'", profile_ids.len(), group_name)
      }]
    }))
  }

  // Full proxy management handlers
  async fn handle_get_proxy(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let proxy_id = arguments
      .get("proxy_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing proxy_id".to_string(),
      })?;

    let proxies = PROXY_MANAGER.get_stored_proxies();
    let proxy = proxies
      .iter()
      .find(|p| p.id == proxy_id)
      .ok_or_else(|| McpError {
        code: -32000,
        message: format!("Proxy not found: {proxy_id}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&proxy).unwrap_or_default()
      }]
    }))
  }

  async fn handle_create_proxy(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing name".to_string(),
      })?;

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    let proxy_type = arguments
      .get("proxy_type")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing proxy_type".to_string(),
      })?;

    let host = arguments
      .get("host")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing host".to_string(),
      })?;

    let port = arguments
      .get("port")
      .and_then(|v| v.as_u64())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing port".to_string(),
      })? as u16;

    let username = arguments
      .get("username")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());
    let password = arguments
      .get("password")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());

    let proxy_settings = ProxySettings {
      proxy_type: proxy_type.to_string(),
      host: host.to_string(),
      port,
      username,
      password,
    };

    let proxy = PROXY_MANAGER
      .create_stored_proxy(app_handle, name.to_string(), proxy_settings)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to create proxy: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Proxy '{}' created successfully with ID: {}", proxy.name, proxy.id)
      }]
    }))
  }

  async fn handle_update_proxy(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let proxy_id = arguments
      .get("proxy_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing proxy_id".to_string(),
      })?;

    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());

    // Build proxy_settings if any settings fields are provided
    let has_settings = arguments.get("proxy_type").is_some()
      || arguments.get("host").is_some()
      || arguments.get("port").is_some();

    let proxy_settings = if has_settings {
      // Get existing proxy to use as defaults
      let proxies = PROXY_MANAGER.get_stored_proxies();
      let existing = proxies
        .iter()
        .find(|p| p.id == proxy_id)
        .ok_or_else(|| McpError {
          code: -32000,
          message: format!("Proxy not found: {proxy_id}"),
        })?;

      let proxy_type = arguments
        .get("proxy_type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| existing.proxy_settings.proxy_type.clone());

      let host = arguments
        .get("host")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| existing.proxy_settings.host.clone());

      let port = arguments
        .get("port")
        .and_then(|v| v.as_u64())
        .map(|p| p as u16)
        .unwrap_or(existing.proxy_settings.port);

      let username = arguments
        .get("username")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| existing.proxy_settings.username.clone());

      let password = arguments
        .get("password")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| existing.proxy_settings.password.clone());

      Some(ProxySettings {
        proxy_type,
        host,
        port,
        username,
        password,
      })
    } else {
      None
    };

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    let proxy = PROXY_MANAGER
      .update_stored_proxy(app_handle, proxy_id, name, proxy_settings)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to update proxy: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Proxy '{}' updated successfully", proxy.name)
      }]
    }))
  }

  async fn handle_delete_proxy(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let proxy_id = arguments
      .get("proxy_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing proxy_id".to_string(),
      })?;

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    PROXY_MANAGER
      .delete_stored_proxy(app_handle, proxy_id)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to delete proxy: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Proxy '{}' deleted successfully", proxy_id)
      }]
    }))
  }

  async fn handle_export_proxies(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let format = arguments
      .get("format")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing format".to_string(),
      })?;

    let content = match format {
      "json" => PROXY_MANAGER.export_proxies_json().map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to export proxies: {e}"),
      })?,
      "txt" => PROXY_MANAGER.export_proxies_txt(),
      _ => {
        return Err(McpError {
          code: -32602,
          message: format!("Invalid format '{}', must be 'json' or 'txt'", format),
        })
      }
    };

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": content
      }]
    }))
  }

  async fn handle_import_proxies(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let content = arguments
      .get("content")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing content".to_string(),
      })?;

    let format = arguments
      .get("format")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing format".to_string(),
      })?;

    let name_prefix = arguments
      .get("name_prefix")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    let result = match format {
      "json" => PROXY_MANAGER
        .import_proxies_json(app_handle, content)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to import proxies: {e}"),
        })?,
      "txt" => {
        use crate::proxy_manager::{ProxyManager, ProxyParseResult};

        let parse_results = ProxyManager::parse_txt_proxies(content);
        let parsed: Vec<_> = parse_results
          .into_iter()
          .filter_map(|r| {
            if let ProxyParseResult::Parsed(p) = r {
              Some(p)
            } else {
              None
            }
          })
          .collect();

        if parsed.is_empty() {
          return Err(McpError {
            code: -32000,
            message: "No valid proxies found in content".to_string(),
          });
        }

        PROXY_MANAGER
          .import_proxies_from_parsed(app_handle, parsed, name_prefix)
          .map_err(|e| McpError {
            code: -32000,
            message: format!("Failed to import proxies: {e}"),
          })?
      }
      _ => {
        return Err(McpError {
          code: -32602,
          message: format!("Invalid format '{}', must be 'json' or 'txt'", format),
        })
      }
    };

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!(
          "Import complete: {} imported, {} skipped, {} errors",
          result.imported_count,
          result.skipped_count,
          result.errors.len()
        )
      }]
    }))
  }

  // VPN management handlers
  async fn handle_import_vpn(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let content = arguments
      .get("content")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing content".to_string(),
      })?;

    let filename = arguments
      .get("filename")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing filename".to_string(),
      })?;

    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .map(|s| s.to_string());

    let storage = crate::vpn::VPN_STORAGE.lock().map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to lock VPN storage: {e}"),
    })?;

    let config = storage
      .import_config(content, filename, name)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to import VPN config: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!(
          "VPN '{}' ({}) imported successfully with ID: {}",
          config.name,
          config.vpn_type,
          config.id
        )
      }]
    }))
  }

  async fn handle_list_vpn_configs(&self) -> Result<serde_json::Value, McpError> {
    let storage = crate::vpn::VPN_STORAGE.lock().map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to lock VPN storage: {e}"),
    })?;

    let configs = storage.list_configs().map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to list VPN configs: {e}"),
    })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&configs).unwrap_or_default()
      }]
    }))
  }

  async fn handle_delete_vpn(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let vpn_id = arguments
      .get("vpn_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing vpn_id".to_string(),
      })?;

    // First disconnect if connected (stop VPN worker)
    let _ = crate::vpn_worker_runner::stop_vpn_worker_by_vpn_id(vpn_id).await;

    let storage = crate::vpn::VPN_STORAGE.lock().map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to lock VPN storage: {e}"),
    })?;

    storage.delete_config(vpn_id).map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to delete VPN config: {e}"),
    })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("VPN '{}' deleted successfully", vpn_id)
      }]
    }))
  }

  async fn handle_connect_vpn(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let vpn_id = arguments
      .get("vpn_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing vpn_id".to_string(),
      })?;

    // Start VPN worker process
    crate::vpn_worker_runner::start_vpn_worker(vpn_id)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to connect VPN: {e}"),
      })?;

    // Update last_used timestamp
    {
      let storage = crate::vpn::VPN_STORAGE.lock().map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to lock VPN storage: {e}"),
      })?;
      let _ = storage.update_last_used(vpn_id);
    }

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("VPN '{}' connected successfully", vpn_id)
      }]
    }))
  }

  async fn handle_disconnect_vpn(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let vpn_id = arguments
      .get("vpn_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing vpn_id".to_string(),
      })?;

    crate::vpn_worker_runner::stop_vpn_worker_by_vpn_id(vpn_id)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to disconnect VPN: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("VPN '{}' disconnected successfully", vpn_id)
      }]
    }))
  }

  async fn handle_get_vpn_status(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let vpn_id = arguments
      .get("vpn_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing vpn_id".to_string(),
      })?;

    let connected =
      if let Some(worker) = crate::vpn_worker_storage::find_vpn_worker_by_vpn_id(vpn_id) {
        worker
          .pid
          .map(crate::proxy_storage::is_process_running)
          .unwrap_or(false)
      } else {
        false
      };

    let status = crate::vpn::VpnStatus {
      connected,
      vpn_id: vpn_id.to_string(),
      connected_at: None,
      bytes_sent: None,
      bytes_received: None,
      last_handshake: None,
    };

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&status).unwrap_or_default()
      }]
    }))
  }

  // Fingerprint management handlers
  async fn handle_get_profile_fingerprint(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = arguments
      .get("profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing profile_id".to_string(),
      })?;

    let profiles = ProfileManager::instance()
      .list_profiles()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list profiles: {e}"),
      })?;

    let profile = profiles
      .iter()
      .find(|p| p.id.to_string() == profile_id)
      .ok_or_else(|| McpError {
        code: -32000,
        message: format!("Profile not found: {profile_id}"),
      })?;

    let fingerprint_info = match profile.browser.as_str() {
      "camoufox" => {
        let config = profile
          .camoufox_config
          .as_ref()
          .cloned()
          .unwrap_or_default();
        serde_json::json!({
          "browser": "camoufox",
          "fingerprint": config.fingerprint,
          "os": config.os,
          "randomize_fingerprint_on_launch": config.randomize_fingerprint_on_launch,
          "screen_max_width": config.screen_max_width,
          "screen_max_height": config.screen_max_height,
          "screen_min_width": config.screen_min_width,
          "screen_min_height": config.screen_min_height,
        })
      }
      browser if is_mcp_chromium_browser(browser) => {
        let config = profile
          .chromium_config
          .as_ref()
          .cloned()
          .unwrap_or_default();
        serde_json::json!({
          "browser": "chromium",
          "fingerprint": config.fingerprint,
          "os": config.os,
          "randomize_fingerprint_on_launch": config.randomize_fingerprint_on_launch,
          "screen_max_width": config.screen_max_width,
          "screen_max_height": config.screen_max_height,
          "screen_min_width": config.screen_min_width,
          "screen_min_height": config.screen_min_height,
        })
      }
      _ => {
        return Err(McpError {
          code: -32000,
          message: "MCP only supports Chromium and Camoufox profiles".to_string(),
        })
      }
    };

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&fingerprint_info).unwrap_or_default()
      }]
    }))
  }

  async fn handle_update_profile_fingerprint(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = arguments
      .get("profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing profile_id".to_string(),
      })?;

    let fingerprint = arguments.get("fingerprint").and_then(|v| v.as_str());
    let os = arguments.get("os").and_then(|v| v.as_str());
    let randomize = arguments
      .get("randomize_fingerprint_on_launch")
      .and_then(|v| v.as_bool());

    let profiles = ProfileManager::instance()
      .list_profiles()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list profiles: {e}"),
      })?;

    let profile = profiles
      .iter()
      .find(|p| p.id.to_string() == profile_id)
      .ok_or_else(|| McpError {
        code: -32000,
        message: format!("Profile not found: {profile_id}"),
      })?;

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    match profile.browser.as_str() {
      "camoufox" => {
        let mut config = profile
          .camoufox_config
          .as_ref()
          .cloned()
          .unwrap_or_default();
        if let Some(fp) = fingerprint {
          config.fingerprint = Some(fp.to_string());
        }
        if let Some(os_val) = os {
          config.os = Some(os_val.to_string());
        }
        if let Some(r) = randomize {
          config.randomize_fingerprint_on_launch = Some(r);
        }
        ProfileManager::instance()
          .update_camoufox_config(app_handle.clone(), profile_id, config)
          .await
          .map_err(|e| McpError {
            code: -32000,
            message: format!("Failed to update camoufox config: {e}"),
          })?;
      }
      browser if is_mcp_chromium_browser(browser) => {
        let mut config = profile
          .chromium_config
          .as_ref()
          .cloned()
          .unwrap_or_default();
        if let Some(fp) = fingerprint {
          config.fingerprint = Some(fp.to_string());
        }
        if let Some(os_val) = os {
          config.os = Some(os_val.to_string());
        }
        if let Some(r) = randomize {
          config.randomize_fingerprint_on_launch = Some(r);
        }
        ProfileManager::instance()
          .update_chromium_config(app_handle.clone(), profile_id, config)
          .await
          .map_err(|e| McpError {
            code: -32000,
            message: format!("Failed to update chromium config: {e}"),
          })?;
      }
      _ => {
        return Err(McpError {
          code: -32000,
          message: "MCP only supports Chromium and Camoufox profiles".to_string(),
        })
      }
    }

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Fingerprint configuration updated for profile '{}'", profile.name)
      }]
    }))
  }

  async fn handle_update_profile_proxy_bypass_rules(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = arguments
      .get("profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing profile_id".to_string(),
      })?;

    let rules: Vec<String> = arguments
      .get("rules")
      .and_then(|v| v.as_array())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing rules array".to_string(),
      })?
      .iter()
      .filter_map(|v| v.as_str().map(|s| s.to_string()))
      .collect();

    let inner = self.inner.lock().await;
    let app_handle = inner.app_handle.as_ref().ok_or_else(|| McpError {
      code: -32000,
      message: "MCP server not properly initialized".to_string(),
    })?;

    let profile = ProfileManager::instance()
      .update_profile_proxy_bypass_rules(app_handle, profile_id, rules.clone())
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to update proxy bypass rules: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!(
          "Proxy bypass rules updated for profile '{}': {} rule(s) configured",
          profile.name,
          rules.len()
        )
      }]
    }))
  }

  async fn handle_update_profile_dns_blocklist(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = arguments
      .get("profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing profile_id".to_string(),
      })?;

    let level = arguments
      .get("level")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing level".to_string(),
      })?;

    let dns_blocklist = if level == "none" {
      None
    } else {
      Some(level.to_string())
    };

    let profile = ProfileManager::instance()
      .update_profile_dns_blocklist(profile_id, dns_blocklist)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to update DNS blocklist: {e}"),
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!(
          "DNS blocklist updated for profile '{}': {}",
          profile.name,
          level
        )
      }]
    }))
  }

  async fn handle_get_dns_blocklist_status(&self) -> Result<serde_json::Value, McpError> {
    let statuses = crate::dns_blocklist::BlocklistManager::get_cache_status();
    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&statuses).unwrap_or_default()
      }]
    }))
  }

  async fn handle_list_extensions(&self) -> Result<serde_json::Value, McpError> {
    let mgr = crate::extension_manager::EXTENSION_MANAGER.lock().unwrap();
    let extensions = mgr.list_extensions().map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to list extensions: {e}"),
    })?;
    Self::json_tool_result(&extensions)
  }

  async fn handle_list_extension_groups(&self) -> Result<serde_json::Value, McpError> {
    let mgr = crate::extension_manager::EXTENSION_MANAGER.lock().unwrap();
    let groups = mgr.list_groups().map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to list extension groups: {e}"),
    })?;
    Self::json_tool_result(&groups)
  }

  async fn handle_create_extension_group(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let name = arguments
      .get("name")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing required parameter: name".to_string(),
      })?;
    let mgr = crate::extension_manager::EXTENSION_MANAGER.lock().unwrap();
    let group = mgr.create_group(name.to_string()).map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to create extension group: {e}"),
    })?;
    Self::json_tool_result(&group)
  }

  async fn handle_delete_extension_mcp(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let extension_id = arguments
      .get("extension_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing required parameter: extension_id".to_string(),
      })?;
    let mgr = crate::extension_manager::EXTENSION_MANAGER.lock().unwrap();
    mgr
      .delete_extension_internal(extension_id)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to delete extension: {e}"),
      })?;
    Self::json_tool_result(&serde_json::json!({ "success": true }))
  }

  async fn handle_delete_extension_group_mcp(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let group_id = arguments
      .get("group_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing required parameter: group_id".to_string(),
      })?;
    let mgr = crate::extension_manager::EXTENSION_MANAGER.lock().unwrap();
    // For MCP, we don't have an app_handle, but we need one for sync deletion.
    // Use the delete_group_internal which skips sync remote deletion.
    mgr.delete_group_internal(group_id).map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to delete extension group: {e}"),
    })?;
    if let Err(e) = crate::events::emit_empty("extensions-changed") {
      log::error!("Failed to emit extensions-changed event: {e}");
    }
    Self::json_tool_result(&serde_json::json!({ "success": true }))
  }

  async fn handle_assign_extension_group_to_profile(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = arguments
      .get("profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing required parameter: profile_id".to_string(),
      })?;
    let extension_group_id = arguments
      .get("extension_group_id")
      .and_then(|v| v.as_str())
      .map(|s| {
        if s.is_empty() {
          None
        } else {
          Some(s.to_string())
        }
      })
      .unwrap_or(None);

    // Validate compatibility if assigning
    if let Some(ref gid) = extension_group_id {
      let profile_manager = ProfileManager::instance();
      let profiles = profile_manager.list_profiles().map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list profiles: {e}"),
      })?;
      let profile = profiles
        .iter()
        .find(|p| p.id.to_string() == profile_id)
        .ok_or_else(|| McpError {
          code: -32000,
          message: format!("Profile '{profile_id}' not found"),
        })?;
      let mgr = crate::extension_manager::EXTENSION_MANAGER.lock().unwrap();
      mgr
        .validate_group_compatibility(gid, &profile.browser)
        .map_err(|e| McpError {
          code: -32000,
          message: format!("{e}"),
        })?;
    }

    let profile_manager = ProfileManager::instance();
    let profile = profile_manager
      .update_profile_extension_group(profile_id, extension_group_id)
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to assign extension group: {e}"),
      })?;
    Self::json_tool_result(&profile)
  }

  async fn handle_get_team_locks(&self) -> Result<serde_json::Value, McpError> {
    if !CLOUD_AUTH.is_on_team_plan().await {
      return Err(McpError {
        code: -32000,
        message: "Team features require an active team plan".to_string(),
      });
    }
    let locks = crate::team_lock::TEAM_LOCK.get_locks().await;
    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&locks).unwrap_or_default()
      }]
    }))
  }

  async fn handle_get_team_lock_status(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    if !CLOUD_AUTH.is_on_team_plan().await {
      return Err(McpError {
        code: -32000,
        message: "Team features require an active team plan".to_string(),
      });
    }
    let profile_id = arguments
      .get("profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing profile_id".to_string(),
      })?;
    let lock_status = crate::team_lock::TEAM_LOCK
      .get_lock_status(profile_id)
      .await;
    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&lock_status).unwrap_or_default()
      }]
    }))
  }

  async fn handle_get_sync_settings(&self) -> Result<serde_json::Value, McpError> {
    let app_handle = self.get_app_handle().await?;
    let settings = crate::settings_manager::get_sync_settings(app_handle)
      .await
      .map_err(McpError::internal)?;
    Self::json_tool_result(&settings)
  }

  async fn handle_save_sync_settings(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: SaveSyncSettingsArgs = Self::parse_arguments(arguments)?;
    if matches!(args.sync_server_url.as_deref(), Some(value) if value.trim().is_empty()) {
      return Err(McpError::invalid_params(
        "sync_server_url cannot be empty or whitespace only",
      ));
    }
    if matches!(args.sync_token.as_deref(), Some(value) if value.trim().is_empty()) {
      return Err(McpError::invalid_params(
        "sync_token cannot be empty or whitespace only",
      ));
    }

    let app_handle = self.get_app_handle().await?;
    let settings = crate::settings_manager::save_sync_settings(
      app_handle,
      args.sync_server_url,
      args.sync_token,
    )
    .await
    .map_err(McpError::internal)?;
    Self::json_tool_result(&settings)
  }

  async fn handle_request_profile_sync(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: RequestProfileSyncArgs = Self::parse_arguments(arguments)?;
    Self::validate_non_empty_key(&args.profile_id, "profile_id")?;
    let app_handle = self.get_app_handle().await?;

    crate::sync::request_profile_sync(app_handle, args.profile_id.clone())
      .await
      .map_err(|error| {
        if error.starts_with("Invalid profile ID:")
          || error == "Sync is not enabled for this profile"
        {
          McpError::invalid_params(error)
        } else if error.starts_with("Profile with ID '") && error.ends_with("' not found") {
          McpError::not_found(error)
        } else {
          McpError::internal(error)
        }
      })?;

    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "status": "queued",
    }))
  }

  async fn handle_validate_recipe(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: ValidateRecipeArgs = Self::parse_arguments(arguments)?;
    let warnings = self.validate_recipe_definition(&args.recipe)?;

    Self::json_tool_result(&serde_json::json!({
      "valid": true,
      "version": args.recipe.version,
      "name": args.recipe.name,
      "stepCount": args.recipe.steps.len(),
      "warnings": warnings,
    }))
  }

  async fn handle_preview_recipe(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: PreviewRecipeArgs = Self::parse_arguments(arguments)?;
    let warnings = self.validate_recipe_definition(&args.recipe)?;

    let steps = args
      .recipe
      .steps
      .iter()
      .enumerate()
      .map(|(index, step)| {
        let retry_safe = Self::recipe_retry_is_safe(&step.tool);
        serde_json::json!({
          "index": index,
          "id": step.id,
          "tool": step.tool,
          "condition": step.condition,
          "output": step.output,
          "onError": step.on_error,
          "retry": step.retry,
          "retrySafe": retry_safe,
          "riskLevel": if retry_safe { "read_like" } else { "stateful" },
          "arguments": step.arguments,
          "effectiveArguments": Self::apply_recipe_defaults(&step.arguments, args.recipe.defaults.as_ref()),
          "replay": {
            "tool": step.tool,
            "arguments": Self::apply_recipe_defaults(&step.arguments, args.recipe.defaults.as_ref()),
          },
        })
      })
      .collect::<Vec<_>>();

    Self::json_tool_result(&serde_json::json!({
      "version": args.recipe.version,
      "name": args.recipe.name,
      "description": args.recipe.description,
      "defaults": args.recipe.defaults,
      "stepCount": args.recipe.steps.len(),
      "warnings": warnings,
      "steps": steps,
    }))
  }

  async fn handle_run_recipe(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: RunRecipeArgs = Self::parse_arguments(arguments)?;
    let warnings = self.validate_recipe_definition(&args.recipe)?;
    let inputs = Self::normalize_recipe_inputs(args.inputs);
    let run_id = Uuid::new_v4().to_string();
    let mut outputs = serde_json::Map::new();
    let mut step_context = serde_json::Map::new();
    let mut step_records = Vec::with_capacity(args.recipe.steps.len());
    let mut successful_steps = 0usize;
    let mut failed_steps = 0usize;
    let mut skipped_steps = 0usize;
    let mut stopped_at_step_id: Option<String> = None;
    let mut final_status = "success".to_string();

    for (index, step) in args.recipe.steps.iter().enumerate() {
      let started_at = std::time::Instant::now();
      let (should_run, condition_report) =
        Self::evaluate_recipe_condition(step.condition.as_ref(), &inputs, &step_context)?;

      if !should_run {
        skipped_steps += 1;
        let duration_ms = started_at.elapsed().as_millis() as u64;
        let record = serde_json::json!({
          "index": index,
          "id": step.id,
          "tool": step.tool,
          "status": "skipped",
          "onError": step.on_error,
          "attempts": 0,
          "durationMs": duration_ms,
          "condition": condition_report,
          "reason": "condition evaluated to false",
          "replay": {
            "tool": step.tool,
            "arguments": serde_json::Value::Null,
          }
        });
        step_context.insert(
          step.id.clone(),
          serde_json::json!({
            "status": "skipped",
            "result": serde_json::Value::Null,
            "error": serde_json::Value::Null,
          }),
        );
        step_records.push(record);
        continue;
      }

      let max_attempts = step
        .retry
        .as_ref()
        .map(|retry| retry.max_attempts)
        .unwrap_or(1);
      let retry_delay_ms = step
        .retry
        .as_ref()
        .and_then(|retry| retry.delay_ms)
        .unwrap_or(0);
      let mut attempts = 0u32;
      let mut resolved_arguments = serde_json::Value::Null;
      let mut raw_response = serde_json::Value::Null;
      let mut normalized_result = serde_json::Value::Null;
      let mut final_error: Option<McpError> = None;
      let mut attempt_errors = Vec::new();

      while attempts < max_attempts {
        attempts += 1;
        let effective_arguments =
          Self::apply_recipe_defaults(&step.arguments, args.recipe.defaults.as_ref());
        match Self::resolve_recipe_value(&effective_arguments, &inputs, &step_context) {
          Ok(arguments) => {
            resolved_arguments = arguments.clone();
            match Box::pin(self.handle_tool_call(Some(serde_json::json!({
              "name": step.tool,
              "arguments": arguments,
            }))))
            .await
            {
              Ok(response) => {
                normalized_result = Self::normalize_recipe_step_result(&response);
                raw_response = response;
                final_error = None;
                break;
              }
              Err(error) => {
                attempt_errors.push(serde_json::json!({
                  "attempt": attempts,
                  "code": error.code,
                  "message": error.message,
                  "stage": "tool_call",
                }));
                final_error = Some(error);
              }
            }
          }
          Err(error) => {
            attempt_errors.push(serde_json::json!({
              "attempt": attempts,
              "code": error.code,
              "message": error.message,
              "stage": "argument_resolution",
            }));
            final_error = Some(error);
          }
        }

        if attempts < max_attempts && retry_delay_ms > 0 {
          tokio::time::sleep(std::time::Duration::from_millis(retry_delay_ms)).await;
        }
      }

      let duration_ms = started_at.elapsed().as_millis() as u64;
      if let Some(error) = final_error {
        failed_steps += 1;
        let status = "failed";
        let error_json = serde_json::json!({
          "code": error.code,
          "message": error.message,
        });
        let record = serde_json::json!({
          "index": index,
          "id": step.id,
          "tool": step.tool,
          "status": status,
          "onError": step.on_error,
          "attempts": attempts,
          "durationMs": duration_ms,
          "condition": condition_report,
          "error": error_json,
          "attemptErrors": attempt_errors,
          "replay": {
            "tool": step.tool,
            "arguments": resolved_arguments,
          }
        });
        step_context.insert(
          step.id.clone(),
          serde_json::json!({
            "status": status,
            "result": serde_json::Value::Null,
            "error": error_json,
          }),
        );
        step_records.push(record);

        if matches!(step.on_error, McpRecipeOnError::Stop) {
          stopped_at_step_id = Some(step.id.clone());
          final_status = "failed".to_string();
          break;
        }

        final_status = "completed_with_errors".to_string();
        continue;
      }

      successful_steps += 1;
      if let Some(output_name) = step.output.as_ref() {
        outputs.insert(output_name.clone(), normalized_result.clone());
      }
      let record = serde_json::json!({
        "index": index,
        "id": step.id,
        "tool": step.tool,
        "status": "success",
        "onError": step.on_error,
        "attempts": attempts,
        "durationMs": duration_ms,
        "condition": condition_report,
        "result": normalized_result,
        "rawResponse": raw_response,
        "output": step.output,
        "replay": {
          "tool": step.tool,
          "arguments": resolved_arguments,
        }
      });
      step_context.insert(
        step.id.clone(),
        serde_json::json!({
          "status": "success",
          "result": record["result"].clone(),
          "error": serde_json::Value::Null,
        }),
      );
      step_records.push(record);
    }

    let completed_steps = successful_steps + failed_steps + skipped_steps;
    if final_status == "success" && failed_steps > 0 {
      final_status = "completed_with_errors".to_string();
    }

    Self::json_tool_result(&serde_json::json!({
      "runId": run_id,
      "recipeName": args.recipe.name,
      "version": args.recipe.version,
      "status": final_status,
      "stepCount": args.recipe.steps.len(),
      "completedSteps": completed_steps,
      "successfulSteps": successful_steps,
      "failedSteps": failed_steps,
      "skippedSteps": skipped_steps,
      "stoppedAtStepId": stopped_at_step_id,
      "warnings": warnings,
      "inputs": inputs,
      "outputs": outputs,
      "steps": step_records,
    }))
  }

  async fn handle_run_batch_profile_workflow(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: RunBatchProfileWorkflowArgs = Self::parse_arguments(arguments)?;
    let warnings = self.validate_recipe_definition(&args.recipe)?;

    if args.profile_ids.is_empty() {
      return Err(McpError::invalid_params(
        "profile_ids cannot be empty for run_batch_profile_workflow",
      ));
    }

    let mut seen_profile_ids = std::collections::HashSet::new();
    for profile_id in &args.profile_ids {
      Self::validate_non_empty_key(profile_id, "profile_ids")?;
      if !seen_profile_ids.insert(profile_id.clone()) {
        return Err(McpError::invalid_params(format!(
          "Duplicate profile_id in run_batch_profile_workflow: {profile_id}"
        )));
      }
    }

    let base_inputs = Self::normalize_recipe_inputs(args.inputs);
    let max_concurrency = args.max_concurrency.unwrap_or(1).max(1);
    let stop_on_profile_failure = args.stop_on_profile_failure.unwrap_or(false);
    let batch_run_id = Uuid::new_v4().to_string();
    let recipe_value =
      serde_json::to_value(&args.recipe).map_err(|e| McpError::internal(format!("{e}")))?;
    let mut results = Vec::new();
    let mut stopped_after_profile_id = None::<String>;
    let mut skipped_profile_ids = Vec::new();

    if stop_on_profile_failure || max_concurrency == 1 {
      for (index, profile_id) in args.profile_ids.iter().enumerate() {
        let inputs = Self::build_profile_recipe_inputs(
          &base_inputs,
          args.per_profile_inputs.as_ref(),
          profile_id,
        );
        let run_result = Box::pin(self.handle_run_recipe(&serde_json::json!({
          "recipe": recipe_value.clone(),
          "inputs": inputs.clone(),
        })))
        .await;

        let profile_report = match run_result {
          Ok(value) => {
            let report = Self::normalize_recipe_step_result(&value);
            let status = report
              .get("status")
              .and_then(|value| value.as_str())
              .unwrap_or("failed")
              .to_string();
            serde_json::json!({
              "index": index,
              "profileId": profile_id,
              "status": status,
              "inputs": inputs,
              "report": report,
            })
          }
          Err(error) => serde_json::json!({
            "index": index,
            "profileId": profile_id,
            "status": "failed",
            "inputs": inputs,
            "error": {
              "code": error.code,
              "message": error.message,
            }
          }),
        };

        let profile_failed = profile_report
          .get("status")
          .and_then(|value| value.as_str())
          .map(|value| value != "success")
          .unwrap_or(true);
        results.push(profile_report);

        if stop_on_profile_failure && profile_failed {
          stopped_after_profile_id = Some(profile_id.clone());
          skipped_profile_ids = args.profile_ids.iter().skip(index + 1).cloned().collect();
          break;
        }
      }
    } else {
      let mut reports = stream::iter(args.profile_ids.iter().cloned().enumerate())
        .map(|(index, profile_id)| {
          let per_profile_inputs = args.per_profile_inputs.clone();
          let base_inputs = base_inputs.clone();
          let recipe_value = recipe_value.clone();
          async move {
            let inputs = Self::build_profile_recipe_inputs(
              &base_inputs,
              per_profile_inputs.as_ref(),
              &profile_id,
            );
            let run_result = Box::pin(self.handle_run_recipe(&serde_json::json!({
              "recipe": recipe_value,
              "inputs": inputs.clone(),
            })))
            .await;
            (index, profile_id, inputs, run_result)
          }
        })
        .buffer_unordered(max_concurrency)
        .collect::<Vec<_>>()
        .await;

      reports.sort_by_key(|(index, _, _, _)| *index);
      for (index, profile_id, inputs, run_result) in reports {
        match run_result {
          Ok(value) => {
            let report = Self::normalize_recipe_step_result(&value);
            let status = report
              .get("status")
              .and_then(|value| value.as_str())
              .unwrap_or("failed")
              .to_string();
            results.push(serde_json::json!({
              "index": index,
              "profileId": profile_id,
              "status": status,
              "inputs": inputs,
              "report": report,
            }));
          }
          Err(error) => results.push(serde_json::json!({
            "index": index,
            "profileId": profile_id,
            "status": "failed",
            "inputs": inputs,
            "error": {
              "code": error.code,
              "message": error.message,
            }
          })),
        }
      }
    }

    let executed_profiles = results.len();
    let successful_profiles = results
      .iter()
      .filter(|result| result.get("status").and_then(|value| value.as_str()) == Some("success"))
      .count();
    let partial_profiles = results
      .iter()
      .filter(|result| {
        result.get("status").and_then(|value| value.as_str()) == Some("completed_with_errors")
      })
      .count();
    let failed_profiles = results
      .iter()
      .filter(|result| {
        matches!(
          result.get("status").and_then(|value| value.as_str()),
          Some("failed") | Some("internal_error")
        )
      })
      .count();

    let status = if failed_profiles > 0 {
      "failed"
    } else if partial_profiles > 0 || !skipped_profile_ids.is_empty() {
      "completed_with_errors"
    } else {
      "success"
    };

    Self::json_tool_result(&serde_json::json!({
      "batchRunId": batch_run_id,
      "recipeName": args.recipe.name,
      "version": args.recipe.version,
      "status": status,
      "maxConcurrency": max_concurrency,
      "stopOnProfileFailure": stop_on_profile_failure,
      "totalProfiles": args.profile_ids.len(),
      "executedProfiles": executed_profiles,
      "successfulProfiles": successful_profiles,
      "partialProfiles": partial_profiles,
      "failedProfiles": failed_profiles,
      "skippedProfileIds": skipped_profile_ids,
      "stoppedAfterProfileId": stopped_after_profile_id,
      "warnings": warnings,
      "results": results,
    }))
  }

  // --- CDP utility methods for browser interaction ---

  fn camoufox_not_supported_for_frames(
    frame: Option<&McpFrameRef>,
    tool_name: &str,
  ) -> Result<(), McpError> {
    if frame.is_some() {
      return Err(McpError::invalid_params(format!(
        "{tool_name} does not yet support frame selection on Camoufox profiles"
      )));
    }
    Ok(())
  }

  fn camoufox_error(context: &str, error: impl std::fmt::Display) -> McpError {
    McpError::internal(format!("Camoufox {context} failed: {error}"))
  }

  async fn get_camoufox_active_page(&self, profile: &BrowserProfile) -> Result<Page, McpError> {
    crate::camoufox_manager::CamoufoxManager::instance()
      .get_active_page(&mcp_profile_path(profile))
      .await
      .map_err(|error| Self::camoufox_error("automation session lookup", error))
  }

  async fn get_camoufox_pages(
    &self,
    profile: &BrowserProfile,
  ) -> Result<(Vec<Page>, usize), McpError> {
    crate::camoufox_manager::CamoufoxManager::instance()
      .list_pages(&mcp_profile_path(profile))
      .await
      .map_err(|error| Self::camoufox_error("tab listing", error))
  }

  async fn build_camoufox_tab_nodes(
    &self,
    pages: Vec<Page>,
    active_index: usize,
  ) -> Result<Vec<McpTabNode>, McpError> {
    let mut tabs = Vec::with_capacity(pages.len());
    for (index, page) in pages.into_iter().enumerate() {
      let title = page
        .title()
        .await
        .map_err(|error| Self::camoufox_error("tab title lookup", error))?;
      let url = page
        .url()
        .map_err(|error| Self::camoufox_error("tab URL lookup", error))?;
      tabs.push(McpTabNode {
        id: crate::camoufox_manager::CamoufoxManager::camoufox_tab_id(index),
        index,
        title,
        url,
        target_type: "page".to_string(),
        active: index == active_index,
      });
    }
    Ok(tabs)
  }

  async fn evaluate_camoufox_page_value(
    &self,
    page: &Page,
    expression: &str,
  ) -> Result<serde_json::Value, McpError> {
    page
      .eval::<serde_json::Value>(expression)
      .await
      .map_err(|error| Self::camoufox_error("page evaluation", error))
  }

  fn camoufox_target_payload(
    selector: Option<&str>,
    locator: Option<&McpLocator>,
  ) -> serde_json::Value {
    if let Some(selector) = selector {
      serde_json::json!({
        "type": "selector",
        "selector": selector,
      })
    } else if let Some(locator) = locator {
      serde_json::json!({
        "type": "locator",
        "locator": locator,
      })
    } else {
      serde_json::Value::Null
    }
  }

  fn camoufox_typing_delay_ms(wpm: Option<f64>) -> Option<f64> {
    let wpm = wpm?;
    if !wpm.is_finite() || wpm <= 0.0 {
      return None;
    }
    Some((60_000.0 / (wpm * 5.0)).clamp(10.0, 400.0))
  }

  async fn wait_for_camoufox_element_actionable(
    &self,
    page: &Page,
    selector: Option<&str>,
    locator: Option<&McpLocator>,
    mode: &str,
    timeout_ms: u64,
  ) -> Result<serde_json::Value, McpError> {
    let expression = Self::build_element_actionability_expression(selector, locator, mode)?;
    let last_observed = Arc::new(Mutex::new(None::<serde_json::Value>));

    self
      .wait_with_timeout(
        timeout_ms,
        || {
          let expression = expression.clone();
          let page = page.clone();
          let last_observed = last_observed.clone();
          async move {
            let value = self
              .evaluate_camoufox_page_value(&page, &expression)
              .await?;
            *last_observed
              .lock()
              .expect("camoufox actionability lock poisoned") = Some(value.clone());

            if let Some(error) = value.get("error").and_then(|value| value.as_str()) {
              return Err(McpError::invalid_params(error));
            }

            if value.get("ready").and_then(|value| value.as_bool()) == Some(true) {
              Ok(Some(value))
            } else {
              Ok(None)
            }
          }
        },
        || {
          let last_observed = last_observed
            .lock()
            .expect("camoufox actionability lock poisoned")
            .clone()
            .unwrap_or(serde_json::Value::Null);
          format!(
            "Timed out after {timeout_ms}ms waiting for {mode} target to become actionable. Last observed state: {}",
            serde_json::to_string(&last_observed).unwrap_or_else(|_| "null".to_string())
          )
        },
      )
      .await
  }

  async fn evaluate_camoufox_element_action(
    &self,
    page: &Page,
    selector: Option<&str>,
    locator: Option<&McpLocator>,
    action_body: &str,
  ) -> Result<serde_json::Value, McpError> {
    let expression = Self::build_element_target_expression(selector, locator, action_body)?;
    self.evaluate_camoufox_page_value(page, &expression).await
  }

  #[allow(dead_code)]
  fn camoufox_find_request_id(
    request_ids: &[(PlaywrightRequest, String)],
    request: &PlaywrightRequest,
  ) -> Option<String> {
    request_ids
      .iter()
      .find(|(candidate, _)| candidate == request)
      .map(|(_, request_id)| request_id.clone())
  }

  #[allow(dead_code)]
  fn playwright_headers_to_json(
    headers: impl IntoIterator<Item = (String, String)>,
  ) -> serde_json::Value {
    let map = headers
      .into_iter()
      .fold(serde_json::Map::new(), |mut acc, (name, value)| {
        acc.insert(name, serde_json::Value::String(value));
        acc
      });
    serde_json::Value::Object(map)
  }

  #[allow(dead_code)]
  fn response_mime_type_from_headers(headers: &serde_json::Value) -> Option<String> {
    headers
      .get("content-type")
      .and_then(|value| value.as_str())
      .and_then(|value| value.split(';').next())
      .map(str::trim)
      .filter(|value| !value.is_empty())
      .map(ToString::to_string)
  }

  fn camoufox_console_capture_bootstrap_script(reset: bool) -> String {
    format!(
      r#"
      (() => {{
        const reset = {reset_flag};
        const storageKey = "__jnm_mcp_console_capture";
        const state = window.__jnmMcpConsoleCapture || {{
          logs: [],
          installed: false,
        }};
        const persist = () => {{
          try {{
            sessionStorage.setItem(storageKey, JSON.stringify({{ logs: state.logs }}));
          }} catch (_error) {{}}
        }};
        if (reset) {{
          state.logs = [];
          try {{
            sessionStorage.removeItem(storageKey);
          }} catch (_error) {{}}
        }}
        if (!state.installed) {{
          const push = (level, args) => {{
            state.logs.push({{
              timestamp: Date.now() / 1000,
              source: "console",
              level,
              text: args.map((value) => {{
                if (typeof value === "string") return value;
                try {{
                  return JSON.stringify(value);
                }} catch (_error) {{
                  return String(value);
                }}
              }}).join(" "),
              url: window.location.href,
              lineNumber: null,
            }});
            persist();
          }};
          for (const level of ["log", "info", "warn", "error", "debug"]) {{
            const original = console[level];
            console[level] = function (...args) {{
              push(level === "warn" ? "warning" : level, args);
              return original.apply(this, args);
            }};
          }}
          window.addEventListener("error", (event) => {{
            state.logs.push({{
              timestamp: Date.now() / 1000,
              source: "pageerror",
              level: "error",
              text: event.message || "Unknown error",
              url: event.filename || window.location.href,
              lineNumber: Number.isFinite(event.lineno) ? event.lineno : null,
            }});
            persist();
          }});
          window.addEventListener("unhandledrejection", (event) => {{
            const reason = event.reason;
            state.logs.push({{
              timestamp: Date.now() / 1000,
              source: "pageerror",
              level: "error",
              text: reason && reason.message ? reason.message : String(reason),
              url: window.location.href,
              lineNumber: null,
            }});
            persist();
          }});
          state.installed = true;
        }}
        window.__jnmMcpConsoleCapture = state;
        persist();
        return true;
      }})()
      "#,
      reset_flag = if reset { "true" } else { "false" }
    )
  }

  fn camoufox_network_capture_bootstrap_script(reset: bool) -> String {
    format!(
      r#"
      (() => {{
        const reset = {reset_flag};
        const storageKey = "__jnm_mcp_network_capture";
        const state = window.__jnmMcpNetworkCapture || {{
          requests: [],
          nextId: 1,
          installed: false,
        }};
        const persist = () => {{
          try {{
            sessionStorage.setItem(storageKey, JSON.stringify({{
              requests: state.requests,
              nextId: state.nextId,
            }}));
          }} catch (_error) {{}}
        }};
        if (reset) {{
          state.requests = [];
          state.nextId = 1;
          try {{
            sessionStorage.removeItem(storageKey);
          }} catch (_error) {{}}
        }}
        const toHeadersObject = (headers) => {{
          const result = {{}};
          if (!headers) return result;
          try {{
            if (typeof headers.forEach === "function") {{
              headers.forEach((value, key) => {{
                result[String(key).toLowerCase()] = String(value);
              }});
              return result;
            }}
          }} catch (_error) {{}}
          if (Array.isArray(headers)) {{
            for (const [key, value] of headers) {{
              result[String(key).toLowerCase()] = String(value);
            }}
          }} else if (typeof headers === "object") {{
            for (const [key, value] of Object.entries(headers)) {{
              result[String(key).toLowerCase()] = String(value);
            }}
          }}
          return result;
        }};
        const bodyToString = async (body) => {{
          if (body == null) return null;
          if (typeof body === "string") return body;
          if (body instanceof URLSearchParams) return body.toString();
          if (body instanceof FormData) {{
            return JSON.stringify(Array.from(body.entries()));
          }}
          if (body instanceof Blob) {{
            return await body.text();
          }}
          if (body instanceof ArrayBuffer) {{
            return btoa(String.fromCharCode(...new Uint8Array(body)));
          }}
          try {{
            return JSON.stringify(body);
          }} catch (_error) {{
            return String(body);
          }}
        }};
        if (!state.installed) {{
          const originalFetch = window.fetch.bind(window);
          window.fetch = async (input, init = undefined) => {{
            const requestId = `fetch-${{state.nextId++}}`;
            const url = typeof input === "string" ? input : (input && input.url) ? input.url : String(input);
            const method = (init && init.method) || (input && input.method) || "GET";
            const requestHeaders = toHeadersObject((init && init.headers) || (input && input.headers));
            const postData = await bodyToString(init && init.body);
            const startedAt = Date.now();
            const entry = {{
              requestId,
              url,
              method,
              resourceType: "fetch",
              initiatorType: "fetch",
              startTimestamp: startedAt / 1000,
              endTimestamp: null,
              durationMs: null,
              status: null,
              statusText: null,
              mimeType: null,
              requestHeaders,
              responseHeaders: {{}},
              postData,
              encodedDataLength: null,
              failed: false,
              errorText: null,
              blockedReason: null,
              hasResponseBody: false,
              responseBody: null,
              responseBodyBase64Encoded: false,
            }};
            state.requests.push(entry);
            persist();
            try {{
              const response = await originalFetch(input, init);
              const clone = response.clone();
              const responseText = await clone.text().catch(() => null);
              const responseHeaders = toHeadersObject(response.headers);
              entry.status = response.status;
              entry.statusText = response.statusText;
              entry.responseHeaders = responseHeaders;
              entry.mimeType = responseHeaders["content-type"] ? responseHeaders["content-type"].split(";")[0].trim() : null;
              entry.responseBody = responseText;
              entry.responseBodyBase64Encoded = false;
              entry.hasResponseBody = responseText !== null;
              entry.encodedDataLength = responseText ? responseText.length : null;
              entry.endTimestamp = Date.now() / 1000;
              entry.durationMs = Date.now() - startedAt;
              persist();
              return response;
            }} catch (error) {{
              entry.failed = true;
              entry.errorText = error && error.message ? error.message : String(error);
              entry.endTimestamp = Date.now() / 1000;
              entry.durationMs = Date.now() - startedAt;
              persist();
              throw error;
            }}
          }};
          state.installed = true;
        }}
        window.__jnmMcpNetworkCapture = state;
        persist();
        return true;
      }})()
      "#,
      reset_flag = if reset { "true" } else { "false" }
    )
  }

  async fn ensure_camoufox_console_capture(
    &self,
    page: &Page,
    reset: bool,
  ) -> Result<(), McpError> {
    let script = Self::camoufox_console_capture_bootstrap_script(reset);
    page
      .add_init_script(&script)
      .await
      .map_err(|error| Self::camoufox_error("console capture bootstrap", error))?;
    page
      .evaluate::<String, serde_json::Value>(
        r#"source => {
          const script = document.createElement("script");
          script.type = "text/javascript";
          script.textContent = source;
          document.documentElement.appendChild(script);
          script.remove();
          return true;
        }"#,
        script,
      )
      .await
      .map_err(|error| Self::camoufox_error("console capture install", error))?;
    Ok(())
  }

  async fn ensure_camoufox_network_capture(
    &self,
    page: &Page,
    reset: bool,
  ) -> Result<(), McpError> {
    let script = Self::camoufox_network_capture_bootstrap_script(reset);
    page
      .add_init_script(&script)
      .await
      .map_err(|error| Self::camoufox_error("network capture bootstrap", error))?;
    page
      .evaluate::<String, serde_json::Value>(
        r#"source => {
          const script = document.createElement("script");
          script.type = "text/javascript";
          script.textContent = source;
          document.documentElement.appendChild(script);
          script.remove();
          return true;
        }"#,
        script,
      )
      .await
      .map_err(|error| Self::camoufox_error("network capture install", error))?;
    Ok(())
  }

  async fn wait_for_camoufox_locator_snapshot(
    &self,
    page: &Page,
    locator: &McpLocator,
    timeout_ms: u64,
  ) -> Result<(LocatorResolutionSnapshot, bool), McpError> {
    let expression = Self::build_locator_resolution_expression(locator)?;
    let last_observed = Arc::new(Mutex::new(None::<serde_json::Value>));

    match self
      .wait_with_timeout(
        timeout_ms,
        || {
          let last_observed = last_observed.clone();
          let expression = expression.clone();
          let page = page.clone();
          async move {
            let value = self
              .evaluate_camoufox_page_value(&page, &expression)
              .await?;
            *last_observed
              .lock()
              .expect("camoufox locator snapshot lock poisoned") = Some(value.clone());

            if let Some(error) = value.get("error").and_then(|value| value.as_str()) {
              return Err(McpError::invalid_params(error));
            }

            let snapshot = Self::parse_locator_resolution_snapshot(value)?;
            if snapshot.exists {
              Ok(Some(snapshot))
            } else {
              Ok(None)
            }
          }
        },
        || {
          let last_observed = last_observed
            .lock()
            .expect("camoufox locator snapshot lock poisoned")
            .clone()
            .unwrap_or(serde_json::Value::Null);
          format!(
            "Timed out after {timeout_ms}ms waiting for locator {:?}. Last observed state: {}",
            locator,
            serde_json::to_string(&last_observed).unwrap_or_else(|_| "null".to_string())
          )
        },
      )
      .await
    {
      Ok(snapshot) => Ok((snapshot, false)),
      Err(error) if error.code == MCP_ERROR_TIMEOUT => {
        let fallback = last_observed
          .lock()
          .expect("camoufox locator snapshot lock poisoned")
          .clone()
          .unwrap_or_else(|| {
            serde_json::json!({
              "locator": locator,
              "strategy": locator.by.as_str(),
              "index": locator.nth.unwrap_or(0),
              "count": 0,
              "exists": false,
              "visible": false,
              "text": "",
              "matchedTexts": [],
            })
          });
        let snapshot = Self::parse_locator_resolution_snapshot(fallback)?;
        Ok((snapshot, true))
      }
      Err(error) => Err(error),
    }
  }

  async fn wait_for_camoufox_locator_collection(
    &self,
    page: &Page,
    locator: &McpLocator,
    limit: usize,
    timeout_ms: u64,
  ) -> Result<(serde_json::Value, bool), McpError> {
    let expression = Self::build_locator_collection_expression(locator, limit)?;
    let last_observed = Arc::new(Mutex::new(None::<serde_json::Value>));

    match self
      .wait_with_timeout(
        timeout_ms,
        || {
          let last_observed = last_observed.clone();
          let expression = expression.clone();
          let page = page.clone();
          async move {
            let value = self.evaluate_camoufox_page_value(&page, &expression).await?;
            *last_observed
              .lock()
              .expect("camoufox locator collection lock poisoned") = Some(value.clone());

            if let Some(error) = value.get("error").and_then(|value| value.as_str()) {
              return Err(McpError::invalid_params(error));
            }

            let count = value.get("count").and_then(|value| value.as_u64()).unwrap_or(0);
            if count > 0 { Ok(Some(value)) } else { Ok(None) }
          }
        },
        || {
          let last_observed = last_observed
            .lock()
            .expect("camoufox locator collection lock poisoned")
            .clone()
            .unwrap_or(serde_json::Value::Null);
          format!(
            "Timed out after {timeout_ms}ms waiting for locator {:?} to return any elements. Last observed state: {}",
            locator,
            serde_json::to_string(&last_observed).unwrap_or_else(|_| "null".to_string())
          )
        },
      )
      .await
    {
      Ok(value) => Ok((value, false)),
      Err(error) if error.code == MCP_ERROR_TIMEOUT => {
        let fallback = last_observed
          .lock()
          .expect("camoufox locator collection lock poisoned")
          .clone()
          .unwrap_or_else(|| {
            serde_json::json!({
              "locator": locator,
              "count": 0,
              "elements": [],
            })
          });
        Ok((fallback, true))
      }
      Err(error) => Err(error),
    }
  }

  async fn get_cdp_port_for_profile(&self, profile: &BrowserProfile) -> Result<u16, McpError> {
    let profiles_dir = ProfileManager::instance().get_profiles_dir();
    let profile_path = profile.get_profile_data_path(&profiles_dir);
    let profile_path_str = profile_path.to_string_lossy();

    // Retry a few times — port info may not be stored yet right after launch
    for attempt in 0..10 {
      if attempt > 0 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
      }
      let port = if is_mcp_chromium_browser(&profile.browser) {
        crate::chromium_manager::ChromiumManager::instance()
          .get_cdp_port(&profile_path_str)
          .await
      } else if profile.browser == "camoufox" {
        crate::camoufox_manager::CamoufoxManager::instance()
          .get_cdp_port(&profile_path_str)
          .await
      } else {
        None
      };
      if let Some(p) = port {
        return Ok(p);
      }
    }

    Err(McpError {
      code: -32000,
      message: format!(
        "No CDP connection available for profile '{}'. Make sure the browser is running.",
        profile.name
      ),
    })
  }

  async fn fetch_cdp_targets(&self, port: u16) -> Result<Vec<serde_json::Value>, McpError> {
    let url = format!("http://127.0.0.1:{port}/json");
    let client = reqwest::Client::new();

    let max_attempts = 15;
    let mut last_err = String::new();
    for attempt in 0..max_attempts {
      if attempt > 0 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
      }
      match client
        .get(&url)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
      {
        Ok(resp) => match resp.json::<Vec<serde_json::Value>>().await {
          Ok(targets) => return Ok(targets),
          Err(e) => {
            last_err = format!("Failed to parse CDP targets: {e}");
          }
        },
        Err(e) => {
          last_err = format!("Failed to connect to browser CDP endpoint: {e}");
        }
      }
    }

    Err(McpError {
      code: -32000,
      message: last_err,
    })
  }

  fn parse_cdp_page_target(value: &serde_json::Value) -> Option<CdpPageTarget> {
    if value.get("type").and_then(|inner| inner.as_str()) != Some("page") {
      return None;
    }

    let id = value
      .get("id")
      .or_else(|| value.get("targetId"))
      .and_then(|inner| inner.as_str())?
      .to_string();

    Some(CdpPageTarget {
      id,
      title: value
        .get("title")
        .and_then(|inner| inner.as_str())
        .unwrap_or_default()
        .to_string(),
      url: value
        .get("url")
        .and_then(|inner| inner.as_str())
        .unwrap_or_default()
        .to_string(),
      target_type: value
        .get("type")
        .and_then(|inner| inner.as_str())
        .unwrap_or("page")
        .to_string(),
      web_socket_debugger_url: value
        .get("webSocketDebuggerUrl")
        .and_then(|inner| inner.as_str())
        .map(ToString::to_string),
    })
  }

  async fn get_cdp_page_targets(&self, port: u16) -> Result<Vec<CdpPageTarget>, McpError> {
    Ok(
      self
        .fetch_cdp_targets(port)
        .await?
        .iter()
        .filter_map(Self::parse_cdp_page_target)
        .collect(),
    )
  }

  async fn set_selected_tab_for_port(&self, port: u16, tab_id: Option<String>) {
    let mut inner = self.inner.lock().await;
    if let Some(tab_id) = tab_id {
      inner.selected_tabs.insert(port, tab_id);
    } else {
      inner.selected_tabs.remove(&port);
    }
  }

  async fn resolve_active_tab_id(
    &self,
    port: u16,
    page_targets: &[CdpPageTarget],
  ) -> Option<String> {
    let fallback = page_targets.first().map(|target| target.id.clone());
    let mut inner = self.inner.lock().await;

    if let Some(selected) = inner.selected_tabs.get(&port).cloned() {
      if page_targets.iter().any(|target| target.id == selected) {
        return Some(selected);
      }
    }

    match fallback {
      Some(tab_id) => {
        inner.selected_tabs.insert(port, tab_id.clone());
        Some(tab_id)
      }
      None => {
        inner.selected_tabs.remove(&port);
        None
      }
    }
  }

  fn build_tab_nodes(
    page_targets: &[CdpPageTarget],
    active_tab_id: Option<&str>,
  ) -> Vec<McpTabNode> {
    page_targets
      .iter()
      .enumerate()
      .map(|(index, target)| McpTabNode {
        id: target.id.clone(),
        index,
        title: target.title.clone(),
        url: target.url.clone(),
        target_type: target.target_type.clone(),
        active: active_tab_id == Some(target.id.as_str()),
      })
      .collect()
  }

  async fn get_cdp_active_page_target(&self, port: u16) -> Result<CdpPageTarget, McpError> {
    let page_targets = self.get_cdp_page_targets(port).await?;
    let active_tab_id = self
      .resolve_active_tab_id(port, &page_targets)
      .await
      .ok_or_else(|| McpError::internal("No page target found in browser"))?;

    page_targets
      .iter()
      .find(|target| target.id == active_tab_id)
      .cloned()
      .ok_or_else(|| McpError::internal("Selected tab is missing from page targets"))
  }

  async fn get_cdp_ws_url(&self, port: u16) -> Result<String, McpError> {
    self
      .get_cdp_active_page_target(port)
      .await?
      .web_socket_debugger_url
      .ok_or_else(|| McpError::internal("Selected tab is missing webSocketDebuggerUrl"))
  }

  async fn activate_tab_target(&self, port: u16, target_id: &str) -> Result<(), McpError> {
    let url = format!(
      "http://127.0.0.1:{port}/json/activate/{}",
      urlencoding::encode(target_id)
    );
    let response = reqwest::Client::new()
      .get(&url)
      .timeout(std::time::Duration::from_secs(5))
      .send()
      .await
      .map_err(|e| {
        McpError::internal(format!("Failed to activate tab via CDP HTTP endpoint: {e}"))
      })?;

    if !response.status().is_success() {
      return Err(McpError::internal(format!(
        "Failed to activate tab: HTTP {}",
        response.status()
      )));
    }

    self
      .set_selected_tab_for_port(port, Some(target_id.to_string()))
      .await;
    Ok(())
  }

  async fn close_tab_target(&self, port: u16, target_id: &str) -> Result<(), McpError> {
    let url = format!(
      "http://127.0.0.1:{port}/json/close/{}",
      urlencoding::encode(target_id)
    );
    let response = reqwest::Client::new()
      .get(&url)
      .timeout(std::time::Duration::from_secs(5))
      .send()
      .await
      .map_err(|e| McpError::internal(format!("Failed to close tab via CDP HTTP endpoint: {e}")))?;

    if !response.status().is_success() {
      return Err(McpError::internal(format!(
        "Failed to close tab: HTTP {}",
        response.status()
      )));
    }

    Ok(())
  }

  async fn create_tab_target(&self, port: u16, url: &str) -> Result<CdpPageTarget, McpError> {
    let endpoint = format!(
      "http://127.0.0.1:{port}/json/new?{}",
      urlencoding::encode(url)
    );
    let client = reqwest::Client::new();
    let mut last_error = None;

    for method in [reqwest::Method::PUT, reqwest::Method::GET] {
      let response = match client
        .request(method.clone(), &endpoint)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
      {
        Ok(response) => response,
        Err(error) => {
          last_error = Some(format!("{} {}", method, error));
          continue;
        }
      };

      if !response.status().is_success() {
        last_error = Some(format!("{} HTTP {}", method, response.status()));
        continue;
      }

      let value = response.json::<serde_json::Value>().await.map_err(|e| {
        McpError::internal(format!(
          "Failed to parse new tab response from CDP HTTP endpoint: {e}"
        ))
      })?;

      if let Some(target) = Self::parse_cdp_page_target(&value) {
        return Ok(target);
      }

      return Err(McpError::internal(
        "CDP new tab endpoint did not return a page target",
      ));
    }

    Err(McpError::internal(format!(
      "Failed to create tab via CDP HTTP endpoint: {}",
      last_error.unwrap_or_else(|| "unknown error".to_string())
    )))
  }

  async fn send_cdp(
    &self,
    ws_url: &str,
    method: &str,
    params: serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    use futures_util::sink::SinkExt;
    use futures_util::stream::StreamExt;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    let (mut ws_stream, _) = connect_async(ws_url).await.map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to connect to CDP WebSocket: {e}"),
    })?;

    let command = serde_json::json!({
      "id": 1,
      "method": method,
      "params": params
    });

    ws_stream
      .send(Message::Text(command.to_string().into()))
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to send CDP command: {e}"),
      })?;

    while let Some(msg) = ws_stream.next().await {
      let msg = msg.map_err(|e| McpError {
        code: -32000,
        message: format!("CDP WebSocket error: {e}"),
      })?;
      if let Message::Text(text) = msg {
        let response: serde_json::Value =
          serde_json::from_str(text.as_str()).map_err(|e| McpError {
            code: -32000,
            message: format!("Failed to parse CDP response: {e}"),
          })?;
        if response.get("id") == Some(&serde_json::json!(1)) {
          if let Some(error) = response.get("error") {
            return Err(McpError {
              code: -32000,
              message: format!("CDP error: {error}"),
            });
          }
          return Ok(
            response
              .get("result")
              .cloned()
              .unwrap_or(serde_json::json!({})),
          );
        }
      }
    }

    Err(McpError {
      code: -32000,
      message: "No response received from CDP".to_string(),
    })
  }

  #[allow(dead_code)]
  async fn get_cdp_response_body(
    &self,
    ws_url: &str,
    request_id: &str,
  ) -> Result<(String, bool), McpError> {
    use futures_util::sink::SinkExt;
    use futures_util::stream::StreamExt;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    let (mut ws_stream, _) = connect_async(ws_url).await.map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to connect to CDP WebSocket: {e}"),
    })?;

    let enable_command = serde_json::json!({
      "id": 1_u64,
      "method": "Network.enable",
      "params": {}
    });
    ws_stream
      .send(Message::Text(enable_command.to_string().into()))
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to send Network.enable: {e}"),
      })?;

    let body_command = serde_json::json!({
      "id": 2_u64,
      "method": "Network.getResponseBody",
      "params": {
        "requestId": request_id
      }
    });
    ws_stream
      .send(Message::Text(body_command.to_string().into()))
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to request response body: {e}"),
      })?;

    while let Some(message) = ws_stream.next().await {
      let message = message.map_err(|e| McpError {
        code: -32000,
        message: format!("CDP WebSocket error: {e}"),
      })?;

      let Message::Text(text) = message else {
        continue;
      };
      let payload: serde_json::Value =
        serde_json::from_str(text.as_str()).map_err(|e| McpError {
          code: -32000,
          message: format!("Failed to parse CDP response body payload: {e}"),
        })?;

      if payload.get("id") == Some(&serde_json::json!(2_u64)) {
        if let Some(error) = payload.get("error") {
          return Err(McpError::not_found(format!(
            "Response body is unavailable for request {request_id}: {error}"
          )));
        }

        let result = payload
          .get("result")
          .cloned()
          .unwrap_or_else(|| serde_json::json!({}));
        let body = result
          .get("body")
          .and_then(|value| value.as_str())
          .unwrap_or_default()
          .to_string();
        let base64_encoded = result
          .get("base64Encoded")
          .and_then(|value| value.as_bool())
          .unwrap_or(false);
        return Ok((body, base64_encoded));
      }
    }

    Err(McpError::internal(format!(
      "No response body received for request {request_id}"
    )))
  }

  async fn send_human_keystrokes(
    &self,
    ws_url: &str,
    text: &str,
    wpm: Option<f64>,
  ) -> Result<(), McpError> {
    use crate::human_typing::{MarkovTyper, TypingAction};
    use futures_util::sink::SinkExt;
    use futures_util::stream::StreamExt;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    let events = MarkovTyper::new(text, wpm).run();

    let (mut ws_stream, _) = connect_async(ws_url).await.map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to connect to CDP WebSocket: {e}"),
    })?;

    let mut cmd_id = 1u64;
    let mut last_time = 0.0;

    for event in &events {
      let delay = event.time - last_time;
      if delay > 0.0 {
        tokio::time::sleep(std::time::Duration::from_secs_f64(delay)).await;
      }
      last_time = event.time;

      match &event.action {
        TypingAction::Char(ch) => {
          let text_str = ch.to_string();
          // keyDown
          let down = serde_json::json!({
            "id": cmd_id,
            "method": "Input.dispatchKeyEvent",
            "params": {
              "type": "keyDown",
              "text": text_str,
              "key": text_str,
              "unmodifiedText": text_str,
            }
          });
          cmd_id += 1;
          ws_stream
            .send(Message::Text(down.to_string().into()))
            .await
            .map_err(|e| McpError {
              code: -32000,
              message: format!("Failed to send key event: {e}"),
            })?;
          // Drain response
          let _ = ws_stream.next().await;

          // keyUp
          let up = serde_json::json!({
            "id": cmd_id,
            "method": "Input.dispatchKeyEvent",
            "params": {
              "type": "keyUp",
              "key": text_str,
            }
          });
          cmd_id += 1;
          ws_stream
            .send(Message::Text(up.to_string().into()))
            .await
            .map_err(|e| McpError {
              code: -32000,
              message: format!("Failed to send key event: {e}"),
            })?;
          let _ = ws_stream.next().await;
        }
        TypingAction::Backspace => {
          let down = serde_json::json!({
            "id": cmd_id,
            "method": "Input.dispatchKeyEvent",
            "params": {
              "type": "keyDown",
              "key": "Backspace",
              "code": "Backspace",
              "windowsVirtualKeyCode": 8,
              "nativeVirtualKeyCode": 8,
            }
          });
          cmd_id += 1;
          ws_stream
            .send(Message::Text(down.to_string().into()))
            .await
            .map_err(|e| McpError {
              code: -32000,
              message: format!("Failed to send key event: {e}"),
            })?;
          let _ = ws_stream.next().await;

          let up = serde_json::json!({
            "id": cmd_id,
            "method": "Input.dispatchKeyEvent",
            "params": {
              "type": "keyUp",
              "key": "Backspace",
              "code": "Backspace",
              "windowsVirtualKeyCode": 8,
              "nativeVirtualKeyCode": 8,
            }
          });
          cmd_id += 1;
          ws_stream
            .send(Message::Text(up.to_string().into()))
            .await
            .map_err(|e| McpError {
              code: -32000,
              message: format!("Failed to send key event: {e}"),
            })?;
          let _ = ws_stream.next().await;
        }
      }
    }

    Ok(())
  }

  async fn dispatch_key_event(
    &self,
    ws_url: &str,
    event_type: &str,
    key: &str,
    code: Option<&str>,
    text: Option<&str>,
    modifiers: Option<i64>,
  ) -> Result<(), McpError> {
    let mut params = serde_json::json!({
      "type": event_type,
      "key": key,
    });
    if let Some(code) = code {
      params["code"] = serde_json::json!(code);
    }
    if let Some(text) = text {
      params["text"] = serde_json::json!(text);
      params["unmodifiedText"] = serde_json::json!(text);
    }
    if let Some(modifiers) = modifiers {
      params["modifiers"] = serde_json::json!(modifiers);
    }

    self
      .send_cdp(ws_url, "Input.dispatchKeyEvent", params)
      .await
      .map(|_| ())
  }

  async fn press_key_sequence(
    &self,
    ws_url: &str,
    key: &str,
    code: Option<&str>,
    text: Option<&str>,
    modifiers: Option<i64>,
  ) -> Result<(), McpError> {
    self
      .dispatch_key_event(ws_url, "keyDown", key, code, text, modifiers)
      .await?;
    self
      .dispatch_key_event(ws_url, "keyUp", key, code, None, modifiers)
      .await
  }

  #[allow(clippy::too_many_arguments)]
  async fn run_element_interaction(
    &self,
    profile_id: &str,
    selector: Option<&str>,
    locator: Option<&McpLocator>,
    frame: Option<&McpFrameRef>,
    timeout_ms: u64,
    auto_wait_enabled: bool,
    wait_mode: &str,
    expression: String,
    wait_for_load: bool,
  ) -> Result<
    (
      Option<McpFrameRef>,
      Option<McpFrameNode>,
      serde_json::Value,
      serde_json::Value,
    ),
    McpError,
  > {
    if let Some(frame) = frame {
      frame.validate()?;
    }
    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    let (frame_selector, resolved_frame, context_id) = self
      .resolve_frame_context(profile_id, &ws_url, frame)
      .await?;
    let readiness = if auto_wait_enabled {
      self
        .wait_for_element_actionable(
          &ws_url, selector, locator, context_id, wait_mode, timeout_ms,
        )
        .await?
    } else {
      serde_json::json!({
        "enabled": false,
        "skipped": true,
      })
    };

    let result = self
      .execute_runtime_expression(&ws_url, &expression, context_id, wait_for_load)
      .await?;
    Ok((frame_selector, resolved_frame, readiness, result))
  }

  /// Send a CDP command and wait for the page to finish loading.
  /// Uses a single WebSocket connection to: enable Page events, send the command,
  /// wait for the command response, then wait for `Page.loadEventFired`.
  async fn send_cdp_and_wait_for_load(
    &self,
    ws_url: &str,
    method: &str,
    params: serde_json::Value,
    timeout_secs: u64,
  ) -> Result<serde_json::Value, McpError> {
    use futures_util::sink::SinkExt;
    use futures_util::stream::StreamExt;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    let (mut ws_stream, _) = connect_async(ws_url).await.map_err(|e| McpError {
      code: -32000,
      message: format!("Failed to connect to CDP WebSocket: {e}"),
    })?;

    // Enable Page domain events so we receive loadEventFired
    let enable_cmd = serde_json::json!({
      "id": 1,
      "method": "Page.enable",
      "params": {}
    });
    ws_stream
      .send(Message::Text(enable_cmd.to_string().into()))
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to send Page.enable: {e}"),
      })?;

    // Wait for Page.enable response
    loop {
      let msg = ws_stream
        .next()
        .await
        .ok_or_else(|| McpError {
          code: -32000,
          message: "WebSocket closed waiting for Page.enable response".to_string(),
        })?
        .map_err(|e| McpError {
          code: -32000,
          message: format!("CDP WebSocket error: {e}"),
        })?;
      if let Message::Text(text) = msg {
        let resp: serde_json::Value = serde_json::from_str(text.as_str()).unwrap_or_default();
        if resp.get("id") == Some(&serde_json::json!(1)) {
          break;
        }
      }
    }

    // Send the actual command (e.g., Page.navigate)
    let command = serde_json::json!({
      "id": 2,
      "method": method,
      "params": params
    });
    ws_stream
      .send(Message::Text(command.to_string().into()))
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to send CDP command: {e}"),
      })?;

    // Wait for command response and then for Page.loadEventFired
    let mut command_result = None;
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);

    loop {
      let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
      if remaining.is_zero() {
        // Timed out waiting for load — return the command result if we have it
        break;
      }

      let msg = match tokio::time::timeout(remaining, ws_stream.next()).await {
        Ok(Some(Ok(msg))) => msg,
        Ok(Some(Err(e))) => {
          return Err(McpError {
            code: -32000,
            message: format!("CDP WebSocket error: {e}"),
          });
        }
        Ok(None) => break, // stream ended
        Err(_) => break,   // timeout
      };

      if let Message::Text(text) = msg {
        let response: serde_json::Value = serde_json::from_str(text.as_str()).unwrap_or_default();

        // Check for command response
        if response.get("id") == Some(&serde_json::json!(2)) {
          if let Some(error) = response.get("error") {
            return Err(McpError {
              code: -32000,
              message: format!("CDP error: {error}"),
            });
          }
          command_result = Some(
            response
              .get("result")
              .cloned()
              .unwrap_or(serde_json::json!({})),
          );
        }

        // Check for Page.loadEventFired — page is fully loaded
        if response.get("method") == Some(&serde_json::json!("Page.loadEventFired")) {
          break;
        }
      }
    }

    // Disable Page domain events
    let disable_cmd = serde_json::json!({
      "id": 3,
      "method": "Page.disable",
      "params": {}
    });
    let _ = ws_stream
      .send(Message::Text(disable_cmd.to_string().into()))
      .await;

    command_result.ok_or_else(|| McpError {
      code: -32000,
      message: "No response received from CDP".to_string(),
    })
  }

  fn get_running_profile(&self, profile_id: &str) -> Result<BrowserProfile, McpError> {
    let profiles = ProfileManager::instance()
      .list_profiles()
      .map_err(|e| McpError {
        code: -32000,
        message: format!("Failed to list profiles: {e}"),
      })?;

    let profile = profiles
      .into_iter()
      .find(|p| p.id.to_string() == profile_id)
      .ok_or_else(|| McpError {
        code: -32000,
        message: format!("Profile not found: {profile_id}"),
      })?;

    if !is_supported_mcp_browser(&profile.browser) {
      return Err(McpError {
        code: -32000,
        message: "MCP only supports Chromium and Camoufox profiles".to_string(),
      });
    }

    if profile.process_id.is_none() {
      return Err(McpError {
        code: -32000,
        message: format!("Profile '{}' is not running", profile.name),
      });
    }

    Ok(profile)
  }

  fn get_profile_download_dir(&self, profile: &BrowserProfile) -> std::path::PathBuf {
    let profiles_dir = ProfileManager::instance().get_profiles_dir();
    profile
      .get_profile_data_path(&profiles_dir)
      .join("mcp-downloads")
  }

  fn pick_download_file_name(url: &reqwest::Url, requested_name: Option<&str>) -> String {
    if let Some(name) = requested_name
      .map(str::trim)
      .filter(|name| !name.is_empty())
    {
      return name.to_string();
    }

    url
      .path_segments()
      .and_then(|segments| segments.rev().find(|segment| !segment.is_empty()))
      .unwrap_or("download.bin")
      .to_string()
  }

  fn unique_download_path(download_dir: &std::path::Path, file_name: &str) -> std::path::PathBuf {
    let candidate = download_dir.join(file_name);
    if !candidate.exists() {
      return candidate;
    }

    let stem = candidate
      .file_stem()
      .and_then(|value| value.to_str())
      .unwrap_or("download");
    let extension = candidate.extension().and_then(|value| value.to_str());

    for index in 1..10_000 {
      let suffixed = match extension {
        Some(extension) if !extension.is_empty() => {
          download_dir.join(format!("{stem}-{index}.{extension}"))
        }
        _ => download_dir.join(format!("{stem}-{index}")),
      };
      if !suffixed.exists() {
        return suffixed;
      }
    }

    download_dir.join(format!("{stem}-{}", Uuid::new_v4()))
  }

  fn cookie_matches_url(cookie: &crate::cookie_manager::UnifiedCookie, url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
      return false;
    };
    let cookie_domain = cookie.domain.trim_start_matches('.');
    if cookie_domain.is_empty() {
      return false;
    }

    let host_matches = host == cookie_domain || host.ends_with(&format!(".{cookie_domain}"));
    let path_matches = url.path().starts_with(&cookie.path);
    let secure_matches = !cookie.is_secure || url.scheme() == "https";

    host_matches && path_matches && secure_matches
  }

  fn build_cookie_header(
    &self,
    profile_id: &str,
    url: &reqwest::Url,
  ) -> Result<Option<String>, McpError> {
    let cookies = match crate::cookie_manager::CookieManager::read_cookies(profile_id) {
      Ok(cookies) => cookies,
      Err(error) if Self::can_fallback_to_empty_cookie_header(&error) => {
        log::warn!(
          "[mcp] Falling back to download without cookies for profile {}: {}",
          profile_id,
          error
        );
        return Ok(None);
      }
      Err(error) => {
        return Err(McpError::internal(format!(
          "Failed to read profile cookies: {error}"
        )));
      }
    };
    let header = cookies
      .domains
      .into_iter()
      .flat_map(|domain| domain.cookies.into_iter())
      .filter(|cookie| Self::cookie_matches_url(cookie, url))
      .map(|cookie| format!("{}={}", cookie.name, cookie.value))
      .collect::<Vec<_>>()
      .join("; ");

    if header.is_empty() {
      Ok(None)
    } else {
      Ok(Some(header))
    }
  }

  fn can_fallback_to_empty_cookie_header(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("database is locked")
      || normalized.contains("database table is locked")
      || normalized.contains("resource temporarily unavailable")
  }

  async fn get_download_record(&self, download_id: &str) -> Result<DownloadRecord, McpError> {
    let state = {
      let inner = self.inner.lock().await;
      inner.downloads.get(download_id).cloned()
    }
    .ok_or_else(|| McpError::not_found(format!("Download not found: {download_id}")))?;

    let record = state.record.lock().await.clone();
    Ok(record)
  }

  async fn insert_download_state(&self, record: DownloadRecord) -> DownloadState {
    let state = DownloadState {
      record: Arc::new(AsyncMutex::new(record.clone())),
    };
    let mut inner = self.inner.lock().await;
    inner
      .downloads
      .insert(record.download_id.clone(), state.clone());
    state
  }

  // --- Browser interaction handlers ---

  async fn handle_navigate(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = arguments
      .get("profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing profile_id".to_string(),
      })?;
    let url = arguments
      .get("url")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing url".to_string(),
      })?;

    let profile = self.get_running_profile(profile_id)?;
    if profile.browser == "camoufox" {
      let page = self.get_camoufox_active_page(&profile).await?;
      page
        .goto_builder(url)
        .goto()
        .await
        .map_err(|error| Self::camoufox_error("navigation", error))?;

      return Ok(serde_json::json!({
        "content": [{
          "type": "text",
          "text": format!("Navigated to {url}")
        }]
      }));
    }
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;

    self
      .send_cdp_and_wait_for_load(
        &ws_url,
        "Page.navigate",
        serde_json::json!({ "url": url }),
        30,
      )
      .await?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": format!("Navigated to {url}")
      }]
    }))
  }

  async fn handle_screenshot(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = arguments
      .get("profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing profile_id".to_string(),
      })?;
    let format = arguments
      .get("format")
      .and_then(|v| v.as_str())
      .unwrap_or("png");
    let quality = arguments.get("quality").and_then(|v| v.as_i64());
    let full_page = arguments
      .get("full_page")
      .and_then(|v| v.as_bool())
      .unwrap_or(false);

    let profile = self.get_running_profile(profile_id)?;
    if profile.browser == "camoufox" {
      let page = self.get_camoufox_active_page(&profile).await?;
      let mut builder = page.screenshot_builder().timeout(30_000.0);

      match format {
        "png" => {}
        "jpeg" | "jpg" | "webp" => {
          return Err(McpError::invalid_params(
            "Camoufox screenshots currently support png output only",
          ));
        }
        other => {
          return Err(McpError::invalid_params(format!(
            "Unsupported screenshot format: {other}"
          )));
        }
      }

      if quality.is_some() {
        return Err(McpError::invalid_params(
          "Camoufox png screenshots do not support quality tuning",
        ));
      }

      if full_page {
        builder = builder.full_page(true);
      }

      let image = builder
        .screenshot()
        .await
        .map_err(|error| Self::camoufox_error("screenshot capture", error))?;
      let data = base64::engine::general_purpose::STANDARD.encode(image);
      return Ok(serde_json::json!({
        "content": [{
          "type": "image",
          "data": data,
          "mimeType": "image/png"
        }]
      }));
    }
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;

    let mut params = serde_json::json!({ "format": format });

    if let Some(q) = quality {
      params["quality"] = serde_json::json!(q);
    }

    if full_page {
      let layout = self
        .send_cdp(&ws_url, "Page.getLayoutMetrics", serde_json::json!({}))
        .await?;

      if let Some(content_size) = layout.get("contentSize") {
        params["clip"] = serde_json::json!({
          "x": 0,
          "y": 0,
          "width": content_size.get("width").and_then(|v| v.as_f64()).unwrap_or(1920.0),
          "height": content_size.get("height").and_then(|v| v.as_f64()).unwrap_or(1080.0),
          "scale": 1
        });
        params["captureBeyondViewport"] = serde_json::json!(true);
      }
    }

    let result = self
      .send_cdp(&ws_url, "Page.captureScreenshot", params)
      .await?;

    let data = result
      .get("data")
      .and_then(|v| v.as_str())
      .unwrap_or_default();

    Ok(serde_json::json!({
      "content": [{
        "type": "image",
        "data": data,
        "mimeType": format!("image/{format}")
      }]
    }))
  }

  async fn handle_evaluate_javascript(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: EvaluateJavascriptArgs = Self::parse_arguments(arguments)?;
    if let Some(frame) = args.frame.as_ref() {
      frame.validate()?;
    }
    let await_promise = args.await_promise.unwrap_or(false);
    let wait_for_load = args.wait_for_load.unwrap_or(false);

    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      Self::camoufox_not_supported_for_frames(args.frame.as_ref(), "evaluate_javascript")?;
      let page = self.get_camoufox_active_page(&profile).await?;
      let result = self
        .evaluate_camoufox_page_value(&page, &args.expression)
        .await?;

      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "frame": serde_json::Value::Null,
        "resolvedFrame": serde_json::Value::Null,
        "awaitPromise": await_promise,
        "waitForLoad": wait_for_load,
        "result": {
          "value": result,
          "type": serde_json::Value::Null,
        },
      }));
    }
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    crate::chromium_manager::ChromiumManager::instance()
      .refresh_runtime_fingerprint_overrides_for_target(&profile, &ws_url)
      .await
      .map_err(|error| {
        McpError::internal(format!(
          "Failed to refresh Chromium runtime fingerprint overrides: {error}"
        ))
      })?;

    let resolved_frame = if let Some(frame) = args.frame.as_ref() {
      Some(
        self
          .resolve_frame_for_profile(&args.profile_id, frame)
          .await?,
      )
    } else {
      None
    };
    let execution_context_id = if let Some(frame) = resolved_frame.as_ref() {
      Some(
        self
          .create_isolated_world_context_id(&ws_url, &frame.id)
          .await?,
      )
    } else {
      None
    };

    let mut cdp_params = serde_json::json!({
      "expression": args.expression,
      "returnByValue": true,
      "awaitPromise": await_promise,
    });
    if let Some(context_id) = execution_context_id {
      cdp_params["contextId"] = serde_json::json!(context_id);
    }

    let result = if wait_for_load {
      self
        .send_cdp_and_wait_for_load(&ws_url, "Runtime.evaluate", cdp_params, 30)
        .await?
    } else {
      self
        .send_cdp(&ws_url, "Runtime.evaluate", cdp_params)
        .await?
    };

    let value = if let Some(exception) = result.get("exceptionDetails") {
      let text = exception
        .get("text")
        .or_else(|| {
          exception
            .get("exception")
            .and_then(|e| e.get("description"))
        })
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown error");
      serde_json::json!({ "error": text })
    } else if let Some(r) = result.get("result") {
      let val = r.get("value").cloned().unwrap_or(serde_json::json!(null));
      serde_json::json!({ "value": val, "type": r.get("type") })
    } else {
      serde_json::json!({ "value": null })
    };

    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "frame": args.frame,
      "resolvedFrame": resolved_frame,
      "awaitPromise": await_promise,
      "waitForLoad": wait_for_load,
      "result": value,
    }))
  }

  async fn handle_click_element(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: ClickElementArgs = Self::parse_arguments(arguments)?;
    Self::validate_element_target(
      args.selector.as_deref(),
      args.locator.as_ref(),
      "click_element",
    )?;
    let auto_wait_enabled = args.options.auto_wait.unwrap_or(true);
    let timeout_ms = args.options.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);

    if let Some(frame) = args.frame.as_ref() {
      frame.validate()?;
    }
    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      Self::camoufox_not_supported_for_frames(args.frame.as_ref(), "click_element")?;
      let page = self.get_camoufox_active_page(&profile).await?;
      let readiness = if auto_wait_enabled {
        self
          .wait_for_camoufox_element_actionable(
            &page,
            args.selector.as_deref(),
            args.locator.as_ref(),
            "click",
            timeout_ms,
          )
          .await?
      } else {
        serde_json::json!({
          "enabled": false,
          "skipped": true,
        })
      };

      if let Some(selector) = args.selector.as_deref() {
        page
          .click_builder(selector)
          .timeout(timeout_ms as f64)
          .click()
          .await
          .map_err(|error| Self::camoufox_error("click", error))?;
      } else {
        let payload = self
          .evaluate_camoufox_element_action(
            &page,
            None,
            args.locator.as_ref(),
            r#"
              element.scrollIntoView({ block: "center" });
              element.click();
              return {
                ok: true,
                target,
                text: getInnerText(element),
              };
            "#,
          )
          .await?;
        if payload.get("ok").and_then(|value| value.as_bool()) != Some(true) {
          let message = payload
            .get("error")
            .and_then(|value| value.as_str())
            .unwrap_or("Click failed");
          return Err(McpError::internal(message));
        }
      }

      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "frame": serde_json::Value::Null,
        "resolvedFrame": serde_json::Value::Null,
        "timeoutMs": timeout_ms,
        "autoWait": readiness,
        "target": Self::camoufox_target_payload(args.selector.as_deref(), args.locator.as_ref()),
        "clicked": true,
      }));
    }
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    let (frame_selector, resolved_frame, context_id) = self
      .resolve_frame_context(&args.profile_id, &ws_url, args.frame.as_ref())
      .await?;
    let readiness = if auto_wait_enabled {
      self
        .wait_for_element_actionable(
          &ws_url,
          args.selector.as_deref(),
          args.locator.as_ref(),
          context_id,
          "click",
          timeout_ms,
        )
        .await?
    } else {
      serde_json::json!({
        "enabled": false,
        "skipped": true,
      })
    };

    let js = Self::build_element_target_expression(
      args.selector.as_deref(),
      args.locator.as_ref(),
      r#"
          element.scrollIntoView({ block: "center" });
          element.click();
          return {
            ok: true,
            target,
            text: getInnerText(element),
          };
      "#,
    )?;

    let mut cdp_params = serde_json::json!({
      "expression": js,
      "returnByValue": true,
    });
    if let Some(context_id) = context_id {
      cdp_params["contextId"] = serde_json::json!(context_id);
    }

    let result = self
      .send_cdp_and_wait_for_load(&ws_url, "Runtime.evaluate", cdp_params, 10)
      .await?;
    let payload = Self::runtime_result_value(&result);

    if payload.get("ok").and_then(|value| value.as_bool()) != Some(true) {
      let message = payload
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or("Click failed");
      return Err(McpError::internal(message));
    }

    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "frame": frame_selector,
      "resolvedFrame": resolved_frame,
      "timeoutMs": timeout_ms,
      "autoWait": readiness,
      "target": payload.get("target").cloned().unwrap_or(serde_json::Value::Null),
      "text": payload.get("text").cloned().unwrap_or(serde_json::Value::Null),
      "clicked": true,
    }))
  }

  async fn handle_type_text(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: TypeTextArgs = Self::parse_arguments(arguments)?;
    Self::validate_element_target(args.selector.as_deref(), args.locator.as_ref(), "type_text")?;
    if args.text.is_empty() {
      return Err(McpError::invalid_params("Text cannot be empty"));
    }
    let clear_first = args.clear_first.unwrap_or(true);
    let instant = args.instant.unwrap_or(false);
    let auto_wait_enabled = args.options.auto_wait.unwrap_or(true);
    let timeout_ms = args.options.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);

    if let Some(frame) = args.frame.as_ref() {
      frame.validate()?;
    }
    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      Self::camoufox_not_supported_for_frames(args.frame.as_ref(), "type_text")?;
      let page = self.get_camoufox_active_page(&profile).await?;
      let readiness = if auto_wait_enabled {
        self
          .wait_for_camoufox_element_actionable(
            &page,
            args.selector.as_deref(),
            args.locator.as_ref(),
            "type",
            timeout_ms,
          )
          .await?
      } else {
        serde_json::json!({
          "enabled": false,
          "skipped": true,
        })
      };

      let focus_payload = self
        .evaluate_camoufox_element_action(
          &page,
          args.selector.as_deref(),
          args.locator.as_ref(),
          if clear_first {
            r#"
              element.scrollIntoView({ block: "center" });
              element.focus();
              if ("value" in element) {
                element.value = "";
              }
              element.dispatchEvent(new Event("input", { bubbles: true }));
              return {
                ok: true,
                target,
                text: getInnerText(element),
                cleared: true,
              };
            "#
          } else {
            r#"
              element.scrollIntoView({ block: "center" });
              element.focus();
              return {
                ok: true,
                target,
                text: getInnerText(element),
                cleared: false,
              };
            "#
          },
        )
        .await?;

      if focus_payload.get("ok").and_then(|value| value.as_bool()) != Some(true) {
        let message = focus_payload
          .get("error")
          .and_then(|value| value.as_str())
          .unwrap_or("Focus failed");
        return Err(McpError::internal(message));
      }

      if instant {
        page
          .keyboard
          .input_text(&args.text)
          .await
          .map_err(|error| Self::camoufox_error("text insertion", error))?;
      } else {
        page
          .keyboard
          .r#type(&args.text, Self::camoufox_typing_delay_ms(args.wpm))
          .await
          .map_err(|error| Self::camoufox_error("typing", error))?;
      }

      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "frame": serde_json::Value::Null,
        "resolvedFrame": serde_json::Value::Null,
        "timeoutMs": timeout_ms,
        "autoWait": readiness,
        "target": focus_payload.get("target").cloned().unwrap_or_else(|| Self::camoufox_target_payload(args.selector.as_deref(), args.locator.as_ref())),
        "textLength": args.text.chars().count(),
        "clearFirst": clear_first,
        "instant": instant,
        "typed": true,
      }));
    }
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    let (frame_selector, resolved_frame, context_id) = self
      .resolve_frame_context(&args.profile_id, &ws_url, args.frame.as_ref())
      .await?;
    let readiness = if auto_wait_enabled {
      self
        .wait_for_element_actionable(
          &ws_url,
          args.selector.as_deref(),
          args.locator.as_ref(),
          context_id,
          "type",
          timeout_ms,
        )
        .await?
    } else {
      serde_json::json!({
        "enabled": false,
        "skipped": true,
      })
    };

    let focus_js = if clear_first {
      Self::build_element_target_expression(
        args.selector.as_deref(),
        args.locator.as_ref(),
        r#"
          element.scrollIntoView({ block: "center" });
          element.focus();
          if ("value" in element) {
            element.value = "";
          }
          element.dispatchEvent(new Event("input", { bubbles: true }));
          return {
            ok: true,
            target,
            text: getInnerText(element),
            value: "value" in element ? String(element.value ?? "") : String(getInnerText(element)),
            cleared: true,
          };
        "#,
      )?
    } else {
      Self::build_element_target_expression(
        args.selector.as_deref(),
        args.locator.as_ref(),
        r#"
          element.scrollIntoView({ block: "center" });
          element.focus();
          return {
            ok: true,
            target,
            text: getInnerText(element),
            value: "value" in element ? String(element.value ?? "") : String(getInnerText(element)),
            cleared: false,
          };
        "#,
      )?
    };

    let mut cdp_params = serde_json::json!({
      "expression": focus_js,
      "returnByValue": true,
    });
    if let Some(context_id) = context_id {
      cdp_params["contextId"] = serde_json::json!(context_id);
    }

    let focus_result = self
      .send_cdp(&ws_url, "Runtime.evaluate", cdp_params)
      .await?;

    let focus_payload = focus_result
      .get("result")
      .and_then(|value| value.get("value"))
      .cloned()
      .unwrap_or(serde_json::Value::Null);

    if focus_payload.get("ok").and_then(|value| value.as_bool()) != Some(true) {
      let message = focus_payload
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or("Focus failed");
      return Err(McpError::internal(message));
    }

    let initial_value = focus_payload
      .get("value")
      .and_then(|value| value.as_str())
      .or_else(|| focus_payload.get("text").and_then(|value| value.as_str()))
      .unwrap_or_default()
      .to_string();

    if instant {
      self
        .send_cdp(
          &ws_url,
          "Input.insertText",
          serde_json::json!({ "text": args.text }),
        )
        .await?;
    } else {
      self
        .send_human_keystrokes(&ws_url, &args.text, args.wpm)
        .await?;
    }

    let expected_value = if clear_first {
      args.text.clone()
    } else {
      format!("{initial_value}{}", args.text)
    };
    let expected_value_literal = serde_json::to_string(&expected_value).map_err(|error| {
      McpError::internal(format!("Failed to serialize expected typed value: {error}"))
    })?;
    let verification_expression = Self::build_element_target_expression(
      args.selector.as_deref(),
      args.locator.as_ref(),
      &format!(
        r#"
          const expectedValue = {expected_value_literal};
          const readValue = () => {{
            if ("value" in element) {{
              return String(element.value ?? "");
            }}
            if (element.isContentEditable) {{
              return String(element.innerText || element.textContent || "");
            }}
            return String(getInnerText(element));
          }};

          let currentValue = readValue();
          let fallbackApplied = false;
          if (currentValue !== expectedValue) {{
            if ("value" in element) {{
              element.value = expectedValue;
            }} else if (element.isContentEditable) {{
              element.textContent = expectedValue;
            }} else {{
              return {{
                ok: false,
                target,
                error: "Element does not expose a writable text value",
                value: currentValue,
                expected: expectedValue,
              }};
            }}
            element.dispatchEvent(new Event("input", {{ bubbles: true }}));
            element.dispatchEvent(new Event("change", {{ bubbles: true }}));
            currentValue = readValue();
            fallbackApplied = true;
          }}

          return {{
            ok: currentValue === expectedValue,
            target,
            value: currentValue,
            expected: expectedValue,
            fallbackApplied,
          }};
      "#
      ),
    )?;
    let verification = self
      .evaluate_runtime_value(&ws_url, &verification_expression, context_id)
      .await?;
    if verification.get("ok").and_then(|value| value.as_bool()) != Some(true) {
      let message = verification
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or("Typed text did not persist on the target element");
      return Err(McpError::internal(message));
    }

    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "frame": frame_selector,
      "resolvedFrame": resolved_frame,
      "timeoutMs": timeout_ms,
      "autoWait": readiness,
      "target": focus_payload.get("target").cloned().unwrap_or(serde_json::Value::Null),
      "value": verification.get("value").cloned().unwrap_or(serde_json::Value::Null),
      "fallbackApplied": verification.get("fallbackApplied").cloned().unwrap_or(serde_json::json!(false)),
      "textLength": args.text.chars().count(),
      "clearFirst": clear_first,
      "instant": instant,
      "typed": true,
    }))
  }

  async fn handle_hover_element(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: ElementInteractionArgs = Self::parse_arguments(arguments)?;
    if let Some(frame) = args.frame.as_ref() {
      frame.validate()?;
    }
    Self::validate_element_target(
      args.selector.as_deref(),
      args.locator.as_ref(),
      "hover_element",
    )?;
    let auto_wait_enabled = args.options.auto_wait.unwrap_or(true);
    let timeout_ms = args.options.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      Self::camoufox_not_supported_for_frames(args.frame.as_ref(), "hover_element")?;
      let page = self.get_camoufox_active_page(&profile).await?;
      let readiness = if auto_wait_enabled {
        self
          .wait_for_camoufox_element_actionable(
            &page,
            args.selector.as_deref(),
            args.locator.as_ref(),
            "click",
            timeout_ms,
          )
          .await?
      } else {
        serde_json::json!({
          "enabled": false,
          "skipped": true,
        })
      };

      if let Some(selector) = args.selector.as_deref() {
        page
          .hover_builder(selector)
          .timeout(timeout_ms as f64)
          .goto()
          .await
          .map_err(|error| Self::camoufox_error("hover", error))?;
      } else {
        let payload = self
          .evaluate_camoufox_element_action(
            &page,
            None,
            args.locator.as_ref(),
            r#"
              element.scrollIntoView({ block: "center", inline: "center" });
              ["mouseover", "mouseenter", "mousemove"].forEach((eventName) => {
                element.dispatchEvent(new MouseEvent(eventName, { bubbles: true, cancelable: true, view: window }));
              });
              return {
                ok: true,
                target,
                hovered: true,
                text: getInnerText(element),
              };
            "#,
          )
          .await?;
        if payload.get("ok").and_then(|value| value.as_bool()) != Some(true) {
          let message = payload
            .get("error")
            .and_then(|value| value.as_str())
            .unwrap_or("Hover failed");
          return Err(McpError::internal(message));
        }
      }

      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "frame": serde_json::Value::Null,
        "resolvedFrame": serde_json::Value::Null,
        "timeoutMs": timeout_ms,
        "autoWait": readiness,
        "target": Self::camoufox_target_payload(args.selector.as_deref(), args.locator.as_ref()),
        "hovered": true,
      }));
    }
    let js = Self::build_element_target_expression(
      args.selector.as_deref(),
      args.locator.as_ref(),
      r#"
        element.scrollIntoView({ block: "center", inline: "center" });
        ["mouseover", "mouseenter", "mousemove"].forEach((eventName) => {
          element.dispatchEvent(new MouseEvent(eventName, { bubbles: true, cancelable: true, view: window }));
        });
        return {
          ok: true,
          target,
          hovered: true,
          text: getInnerText(element),
        };
      "#,
    )?;
    let (frame_selector, resolved_frame, readiness, result) = self
      .run_element_interaction(
        &args.profile_id,
        args.selector.as_deref(),
        args.locator.as_ref(),
        args.frame.as_ref(),
        timeout_ms,
        auto_wait_enabled,
        "click",
        js,
        false,
      )
      .await?;
    let payload = result
      .get("result")
      .and_then(|value| value.get("value"))
      .cloned()
      .unwrap_or(serde_json::Value::Null);
    if payload.get("ok").and_then(|value| value.as_bool()) != Some(true) {
      let message = payload
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or("Hover failed");
      return Err(McpError::internal(message));
    }
    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "frame": frame_selector,
      "resolvedFrame": resolved_frame,
      "timeoutMs": timeout_ms,
      "autoWait": readiness,
      "target": payload.get("target").cloned().unwrap_or(serde_json::Value::Null),
      "text": payload.get("text").cloned().unwrap_or(serde_json::Value::Null),
      "hovered": true,
    }))
  }

  async fn handle_focus_element(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: ElementInteractionArgs = Self::parse_arguments(arguments)?;
    if let Some(frame) = args.frame.as_ref() {
      frame.validate()?;
    }
    Self::validate_element_target(
      args.selector.as_deref(),
      args.locator.as_ref(),
      "focus_element",
    )?;
    let auto_wait_enabled = args.options.auto_wait.unwrap_or(true);
    let timeout_ms = args.options.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      Self::camoufox_not_supported_for_frames(args.frame.as_ref(), "focus_element")?;
      let page = self.get_camoufox_active_page(&profile).await?;
      let readiness = if auto_wait_enabled {
        self
          .wait_for_camoufox_element_actionable(
            &page,
            args.selector.as_deref(),
            args.locator.as_ref(),
            "type",
            timeout_ms,
          )
          .await?
      } else {
        serde_json::json!({
          "enabled": false,
          "skipped": true,
        })
      };

      let payload = if let Some(selector) = args.selector.as_deref() {
        page
          .focus(selector, Some(timeout_ms as f64))
          .await
          .map_err(|error| Self::camoufox_error("focus", error))?;
        serde_json::json!({
          "ok": true,
          "target": {
            "type": "selector",
            "selector": selector,
          },
          "focused": true,
        })
      } else {
        self
          .evaluate_camoufox_element_action(
            &page,
            None,
            args.locator.as_ref(),
            r#"
              element.scrollIntoView({ block: "center", inline: "center" });
              if (typeof element.focus === "function") {
                element.focus();
              }
              return {
                ok: document.activeElement === element,
                target,
                focused: document.activeElement === element,
                error: document.activeElement === element ? null : "focus_failed",
              };
            "#,
          )
          .await?
      };

      if payload.get("ok").and_then(|value| value.as_bool()) != Some(true) {
        let message = payload
          .get("error")
          .and_then(|value| value.as_str())
          .unwrap_or("Focus failed");
        return Err(McpError::internal(message));
      }

      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "frame": serde_json::Value::Null,
        "resolvedFrame": serde_json::Value::Null,
        "timeoutMs": timeout_ms,
        "autoWait": readiness,
        "target": payload.get("target").cloned().unwrap_or_else(|| Self::camoufox_target_payload(args.selector.as_deref(), args.locator.as_ref())),
        "focused": true,
      }));
    }
    let js = Self::build_element_target_expression(
      args.selector.as_deref(),
      args.locator.as_ref(),
      r#"
        element.scrollIntoView({ block: "center", inline: "center" });
        if (typeof element.focus === "function") {
          element.focus();
        }
        return {
          ok: document.activeElement === element,
          target,
          focused: document.activeElement === element,
          text: getInnerText(element),
          error: document.activeElement === element ? null : "focus_failed",
        };
      "#,
    )?;
    let (frame_selector, resolved_frame, readiness, result) = self
      .run_element_interaction(
        &args.profile_id,
        args.selector.as_deref(),
        args.locator.as_ref(),
        args.frame.as_ref(),
        timeout_ms,
        auto_wait_enabled,
        "click",
        js,
        false,
      )
      .await?;
    let payload = result
      .get("result")
      .and_then(|value| value.get("value"))
      .cloned()
      .unwrap_or(serde_json::Value::Null);
    if payload.get("ok").and_then(|value| value.as_bool()) != Some(true) {
      let message = payload
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or("Focus failed");
      return Err(McpError::internal(message));
    }
    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "frame": frame_selector,
      "resolvedFrame": resolved_frame,
      "timeoutMs": timeout_ms,
      "autoWait": readiness,
      "target": payload.get("target").cloned().unwrap_or(serde_json::Value::Null),
      "focused": true,
    }))
  }

  async fn handle_scroll_to(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: ScrollToArgs = Self::parse_arguments(arguments)?;
    if let Some(frame) = args.frame.as_ref() {
      frame.validate()?;
    }
    let behavior = args.behavior.as_deref().unwrap_or("auto");
    if !matches!(behavior, "auto" | "smooth" | "instant") {
      return Err(McpError::invalid_params(format!(
        "Unsupported scroll behavior: {behavior}"
      )));
    }
    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      Self::camoufox_not_supported_for_frames(args.frame.as_ref(), "scroll_to")?;
      let page = self.get_camoufox_active_page(&profile).await?;
      let x = args.x.unwrap_or(0.0);
      let y = args.y.unwrap_or(0.0);
      let expression = format!(
        "(() => {{ window.scrollTo({{ left: {}, top: {}, behavior: {} }}); return {{ x: window.scrollX, y: window.scrollY }}; }})()",
        serde_json::to_string(&x).map_err(|e| McpError::internal(format!("Failed to serialize x: {e}")))?,
        serde_json::to_string(&y).map_err(|e| McpError::internal(format!("Failed to serialize y: {e}")))?,
        serde_json::to_string(behavior).map_err(|e| McpError::internal(format!("Failed to serialize behavior: {e}")))?,
      );
      let position = self
        .evaluate_camoufox_page_value(&page, &expression)
        .await?;
      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "frame": serde_json::Value::Null,
        "resolvedFrame": serde_json::Value::Null,
        "behavior": behavior,
        "position": position,
        "scrolled": true,
      }));
    }
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    let (frame_selector, resolved_frame, context_id) = self
      .resolve_frame_context(&args.profile_id, &ws_url, args.frame.as_ref())
      .await?;
    let x = args.x.unwrap_or(0.0);
    let y = args.y.unwrap_or(0.0);
    let expression = format!(
      "(() => {{ window.scrollTo({{ left: {}, top: {}, behavior: {} }}); return {{ x: window.scrollX, y: window.scrollY }}; }})()",
      serde_json::to_string(&x).map_err(|e| McpError::internal(format!("Failed to serialize x: {e}")))?,
      serde_json::to_string(&y).map_err(|e| McpError::internal(format!("Failed to serialize y: {e}")))?,
      serde_json::to_string(behavior).map_err(|e| McpError::internal(format!("Failed to serialize behavior: {e}")))?,
    );
    let position = self
      .evaluate_runtime_value(&ws_url, &expression, context_id)
      .await?;
    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "frame": frame_selector,
      "resolvedFrame": resolved_frame,
      "behavior": behavior,
      "position": position,
      "scrolled": true,
    }))
  }

  async fn handle_scroll_element_into_view(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: ElementInteractionArgs = Self::parse_arguments(arguments)?;
    if let Some(frame) = args.frame.as_ref() {
      frame.validate()?;
    }
    Self::validate_element_target(
      args.selector.as_deref(),
      args.locator.as_ref(),
      "scroll_element_into_view",
    )?;
    let auto_wait_enabled = args.options.auto_wait.unwrap_or(true);
    let timeout_ms = args.options.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      Self::camoufox_not_supported_for_frames(args.frame.as_ref(), "scroll_element_into_view")?;
      let page = self.get_camoufox_active_page(&profile).await?;
      let readiness = if auto_wait_enabled {
        self
          .wait_for_camoufox_element_actionable(
            &page,
            args.selector.as_deref(),
            args.locator.as_ref(),
            "click",
            timeout_ms,
          )
          .await?
      } else {
        serde_json::json!({
          "enabled": false,
          "skipped": true,
        })
      };
      let payload = self
        .evaluate_camoufox_element_action(
          &page,
          args.selector.as_deref(),
          args.locator.as_ref(),
          r#"
            element.scrollIntoView({ block: "center", inline: "center", behavior: "auto" });
            return {
              ok: true,
              target,
              scrolled: true,
              rect: element.getBoundingClientRect(),
            };
          "#,
        )
        .await?;
      if payload.get("ok").and_then(|value| value.as_bool()) != Some(true) {
        let message = payload
          .get("error")
          .and_then(|value| value.as_str())
          .unwrap_or("Scroll into view failed");
        return Err(McpError::internal(message));
      }
      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "frame": serde_json::Value::Null,
        "resolvedFrame": serde_json::Value::Null,
        "timeoutMs": timeout_ms,
        "autoWait": readiness,
        "target": payload.get("target").cloned().unwrap_or_else(|| Self::camoufox_target_payload(args.selector.as_deref(), args.locator.as_ref())),
        "rect": payload.get("rect").cloned().unwrap_or(serde_json::Value::Null),
        "scrolled": true,
      }));
    }
    let js = Self::build_element_target_expression(
      args.selector.as_deref(),
      args.locator.as_ref(),
      r#"
        element.scrollIntoView({ block: "center", inline: "center", behavior: "auto" });
        return {
          ok: true,
          target,
          scrolled: true,
          rect: element.getBoundingClientRect(),
        };
      "#,
    )?;
    let (frame_selector, resolved_frame, readiness, result) = self
      .run_element_interaction(
        &args.profile_id,
        args.selector.as_deref(),
        args.locator.as_ref(),
        args.frame.as_ref(),
        timeout_ms,
        auto_wait_enabled,
        "click",
        js,
        false,
      )
      .await?;
    let payload = result
      .get("result")
      .and_then(|value| value.get("value"))
      .cloned()
      .unwrap_or(serde_json::Value::Null);
    if payload.get("ok").and_then(|value| value.as_bool()) != Some(true) {
      let message = payload
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or("Scroll into view failed");
      return Err(McpError::internal(message));
    }
    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "frame": frame_selector,
      "resolvedFrame": resolved_frame,
      "timeoutMs": timeout_ms,
      "autoWait": readiness,
      "target": payload.get("target").cloned().unwrap_or(serde_json::Value::Null),
      "rect": payload.get("rect").cloned().unwrap_or(serde_json::Value::Null),
      "scrolled": true,
    }))
  }

  async fn handle_press_key(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: PressKeyArgs = Self::parse_arguments(arguments)?;
    Self::validate_non_empty_key(&args.key, "key")?;
    if let Some(frame) = args.frame.as_ref() {
      frame.validate()?;
    }
    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      Self::camoufox_not_supported_for_frames(args.frame.as_ref(), "press_key")?;
      let page = self.get_camoufox_active_page(&profile).await?;
      page
        .keyboard
        .down(&args.key)
        .await
        .map_err(|error| Self::camoufox_error("key down", error))?;
      page
        .keyboard
        .up(&args.key)
        .await
        .map_err(|error| Self::camoufox_error("key up", error))?;
      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "frame": serde_json::Value::Null,
        "resolvedFrame": serde_json::Value::Null,
        "key": args.key,
        "code": args.code,
        "text": args.text,
        "pressed": true,
      }));
    }
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    let (frame_selector, resolved_frame, context_id) = self
      .resolve_frame_context(&args.profile_id, &ws_url, args.frame.as_ref())
      .await?;
    if context_id.is_some() {
      let _ = self
        .evaluate_runtime_value(
          &ws_url,
          "(() => { window.focus(); if (document.body && typeof document.body.focus === 'function') document.body.focus(); return true; })()",
          context_id,
        )
        .await?;
    }
    self
      .press_key_sequence(
        &ws_url,
        &args.key,
        args.code.as_deref(),
        args.text.as_deref(),
        None,
      )
      .await?;
    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "frame": frame_selector,
      "resolvedFrame": resolved_frame,
      "key": args.key,
      "code": args.code,
      "text": args.text,
      "pressed": true,
    }))
  }

  async fn handle_press_hotkey(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: PressHotkeyArgs = Self::parse_arguments(arguments)?;
    if args.keys.is_empty() {
      return Err(McpError::invalid_params("keys cannot be empty"));
    }
    for key in &args.keys {
      Self::validate_non_empty_key(key, "keys item")?;
    }
    if let Some(frame) = args.frame.as_ref() {
      frame.validate()?;
    }
    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      Self::camoufox_not_supported_for_frames(args.frame.as_ref(), "press_hotkey")?;
      let page = self.get_camoufox_active_page(&profile).await?;
      for key in &args.keys {
        page
          .keyboard
          .down(key)
          .await
          .map_err(|error| Self::camoufox_error("hotkey keyDown", error))?;
      }
      for key in args.keys.iter().rev() {
        page
          .keyboard
          .up(key)
          .await
          .map_err(|error| Self::camoufox_error("hotkey keyUp", error))?;
      }
      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "frame": serde_json::Value::Null,
        "resolvedFrame": serde_json::Value::Null,
        "keys": args.keys,
        "pressed": true,
      }));
    }
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    let (frame_selector, resolved_frame, context_id) = self
      .resolve_frame_context(&args.profile_id, &ws_url, args.frame.as_ref())
      .await?;
    if context_id.is_some() {
      let _ = self
        .evaluate_runtime_value(
          &ws_url,
          "(() => { window.focus(); if (document.body && typeof document.body.focus === 'function') document.body.focus(); return true; })()",
          context_id,
        )
        .await?;
    }
    for key in &args.keys {
      self
        .dispatch_key_event(&ws_url, "keyDown", key, None, None, None)
        .await?;
    }
    for key in args.keys.iter().rev() {
      self
        .dispatch_key_event(&ws_url, "keyUp", key, None, None, None)
        .await?;
    }
    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "frame": frame_selector,
      "resolvedFrame": resolved_frame,
      "keys": args.keys,
      "pressed": true,
    }))
  }

  async fn handle_select_option(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: SelectOptionArgs = Self::parse_arguments(arguments)?;
    Self::validate_select_option_args(&args)?;
    let auto_wait_enabled = args.options.auto_wait.unwrap_or(true);
    let timeout_ms = args.options.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      Self::camoufox_not_supported_for_frames(args.frame.as_ref(), "select_option")?;
      let page = self.get_camoufox_active_page(&profile).await?;
      let readiness = if auto_wait_enabled {
        self
          .wait_for_camoufox_element_actionable(
            &page,
            args.selector.as_deref(),
            args.locator.as_ref(),
            "click",
            timeout_ms,
          )
          .await?
      } else {
        serde_json::json!({
          "enabled": false,
          "skipped": true,
        })
      };

      let payload = if let Some(selector) = args.selector.as_deref() {
        let mut builder = page
          .select_option_builder(selector)
          .timeout(timeout_ms as f64);
        if let Some(value) = &args.value {
          builder = builder.add_value(value.clone());
        } else if let Some(label) = &args.label {
          builder = builder.add_label(label.clone());
        } else if let Some(index) = args.index {
          builder = builder.add_index(index);
        }
        builder
          .select_option()
          .await
          .map_err(|error| Self::camoufox_error("select option", error))?;

        let value = page
          .evaluate_on_selector::<serde_json::Value, serde_json::Value>(
            selector,
            "element => ({ selectedIndex: element.selectedIndex, value: element.value, label: element.options[element.selectedIndex]?.label ?? element.options[element.selectedIndex]?.text ?? null })",
            None::<serde_json::Value>,
          )
          .await
          .map_err(|error| Self::camoufox_error("selected option readback", error))?;
        serde_json::json!({
          "ok": true,
          "target": {
            "type": "selector",
            "selector": selector,
          },
          "selectedIndex": value.get("selectedIndex").cloned().unwrap_or(serde_json::Value::Null),
          "value": value.get("value").cloned().unwrap_or(serde_json::Value::Null),
          "label": value.get("label").cloned().unwrap_or(serde_json::Value::Null),
        })
      } else {
        self
          .evaluate_camoufox_element_action(
            &page,
            None,
            args.locator.as_ref(),
            &format!(
              r#"
                if (!(element instanceof HTMLSelectElement)) {{
                  return {{ ok: false, error: "Target is not a <select> element", target }};
                }}
                const wantedValue = {};
                const wantedLabel = {};
                const wantedIndex = {};
                let matchedIndex = -1;
                if (typeof wantedValue === "string") {{
                  matchedIndex = Array.from(element.options).findIndex((option) => option.value === wantedValue);
                }} else if (typeof wantedLabel === "string") {{
                  matchedIndex = Array.from(element.options).findIndex((option) => option.label === wantedLabel || option.text === wantedLabel);
                }} else if (Number.isInteger(wantedIndex)) {{
                  matchedIndex = wantedIndex;
                }}
                if (matchedIndex < 0 || matchedIndex >= element.options.length) {{
                  return {{ ok: false, error: "Requested option was not found", target }};
                }}
                element.selectedIndex = matchedIndex;
                element.dispatchEvent(new Event("input", {{ bubbles: true }}));
                element.dispatchEvent(new Event("change", {{ bubbles: true }}));
                return {{
                  ok: true,
                  target,
                  selectedIndex: element.selectedIndex,
                  value: element.value,
                  label: element.options[element.selectedIndex]?.label ?? element.options[element.selectedIndex]?.text ?? null,
                }};
              "#,
              serde_json::to_string(&args.value).map_err(|e| McpError::internal(format!("Failed to serialize select value: {e}")))?,
              serde_json::to_string(&args.label).map_err(|e| McpError::internal(format!("Failed to serialize select label: {e}")))?,
              serde_json::to_string(&args.index).map_err(|e| McpError::internal(format!("Failed to serialize select index: {e}")))?,
            ),
          )
          .await?
      };

      if payload.get("ok").and_then(|value| value.as_bool()) != Some(true) {
        let message = payload
          .get("error")
          .and_then(|value| value.as_str())
          .unwrap_or("Select option failed");
        return Err(McpError::internal(message));
      }

      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "frame": serde_json::Value::Null,
        "resolvedFrame": serde_json::Value::Null,
        "timeoutMs": timeout_ms,
        "autoWait": readiness,
        "target": payload.get("target").cloned().unwrap_or_else(|| Self::camoufox_target_payload(args.selector.as_deref(), args.locator.as_ref())),
        "selectedIndex": payload.get("selectedIndex").cloned().unwrap_or(serde_json::Value::Null),
        "value": payload.get("value").cloned().unwrap_or(serde_json::Value::Null),
        "label": payload.get("label").cloned().unwrap_or(serde_json::Value::Null),
        "selected": true,
      }));
    }
    let value_literal = serde_json::to_string(&args.value)
      .map_err(|e| McpError::internal(format!("Failed to serialize select value: {e}")))?;
    let label_literal = serde_json::to_string(&args.label)
      .map_err(|e| McpError::internal(format!("Failed to serialize select label: {e}")))?;
    let index_literal = serde_json::to_string(&args.index)
      .map_err(|e| McpError::internal(format!("Failed to serialize select index: {e}")))?;
    let js = Self::build_element_target_expression(
      args.selector.as_deref(),
      args.locator.as_ref(),
      &format!(
        r#"
        if (!(element instanceof HTMLSelectElement)) {{
          return {{ ok: false, error: "Target is not a <select> element", target }};
        }}
        const wantedValue = {value_literal};
        const wantedLabel = {label_literal};
        const wantedIndex = {index_literal};
        let matchedIndex = -1;
        if (typeof wantedValue === "string") {{
          matchedIndex = Array.from(element.options).findIndex((option) => option.value === wantedValue);
        }} else if (typeof wantedLabel === "string") {{
          matchedIndex = Array.from(element.options).findIndex((option) => option.label === wantedLabel || option.text === wantedLabel);
        }} else if (Number.isInteger(wantedIndex)) {{
          matchedIndex = wantedIndex;
        }}
        if (matchedIndex < 0 || matchedIndex >= element.options.length) {{
          return {{ ok: false, error: "Requested option was not found", target }};
        }}
        element.selectedIndex = matchedIndex;
        element.dispatchEvent(new Event("input", {{ bubbles: true }}));
        element.dispatchEvent(new Event("change", {{ bubbles: true }}));
        return {{
          ok: true,
          target,
          selectedIndex: element.selectedIndex,
          value: element.value,
          label: element.options[element.selectedIndex]?.label ?? element.options[element.selectedIndex]?.text ?? null,
        }};
      "#
      ),
    )?;
    let (frame_selector, resolved_frame, readiness, result) = self
      .run_element_interaction(
        &args.profile_id,
        args.selector.as_deref(),
        args.locator.as_ref(),
        args.frame.as_ref(),
        timeout_ms,
        auto_wait_enabled,
        "click",
        js,
        false,
      )
      .await?;
    let payload = result
      .get("result")
      .and_then(|value| value.get("value"))
      .cloned()
      .unwrap_or(serde_json::Value::Null);
    if payload.get("ok").and_then(|value| value.as_bool()) != Some(true) {
      let message = payload
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or("Select option failed");
      return Err(McpError::internal(message));
    }
    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "frame": frame_selector,
      "resolvedFrame": resolved_frame,
      "timeoutMs": timeout_ms,
      "autoWait": readiness,
      "target": payload.get("target").cloned().unwrap_or(serde_json::Value::Null),
      "selectedIndex": payload.get("selectedIndex").cloned().unwrap_or(serde_json::Value::Null),
      "value": payload.get("value").cloned().unwrap_or(serde_json::Value::Null),
      "label": payload.get("label").cloned().unwrap_or(serde_json::Value::Null),
      "selected": true,
    }))
  }

  async fn handle_check_checkbox(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    self.handle_toggle_checkbox(arguments, true).await
  }

  async fn handle_uncheck_checkbox(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    self.handle_toggle_checkbox(arguments, false).await
  }

  async fn handle_toggle_checkbox(
    &self,
    arguments: &serde_json::Value,
    desired_checked: bool,
  ) -> Result<serde_json::Value, McpError> {
    let args: ElementInteractionArgs = Self::parse_arguments(arguments)?;
    let tool_name = if desired_checked {
      "check_checkbox"
    } else {
      "uncheck_checkbox"
    };
    if let Some(frame) = args.frame.as_ref() {
      frame.validate()?;
    }
    Self::validate_element_target(args.selector.as_deref(), args.locator.as_ref(), tool_name)?;
    let auto_wait_enabled = args.options.auto_wait.unwrap_or(true);
    let timeout_ms = args.options.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      Self::camoufox_not_supported_for_frames(args.frame.as_ref(), tool_name)?;
      let page = self.get_camoufox_active_page(&profile).await?;
      let readiness = if auto_wait_enabled {
        self
          .wait_for_camoufox_element_actionable(
            &page,
            args.selector.as_deref(),
            args.locator.as_ref(),
            "click",
            timeout_ms,
          )
          .await?
      } else {
        serde_json::json!({
          "enabled": false,
          "skipped": true,
        })
      };

      let payload = if let Some(selector) = args.selector.as_deref() {
        if desired_checked {
          page
            .check_builder(selector)
            .timeout(timeout_ms as f64)
            .check()
            .await
            .map_err(|error| Self::camoufox_error("checkbox check", error))?;
        } else {
          page
            .uncheck_builder(selector)
            .timeout(timeout_ms as f64)
            .uncheck()
            .await
            .map_err(|error| Self::camoufox_error("checkbox uncheck", error))?;
        }
        let checked = page
          .evaluate_on_selector::<serde_json::Value, bool>(
            selector,
            "element => !!element.checked",
            None::<serde_json::Value>,
          )
          .await
          .map_err(|error| Self::camoufox_error("checkbox state readback", error))?;
        serde_json::json!({
          "ok": true,
          "target": {
            "type": "selector",
            "selector": selector,
          },
          "changed": true,
          "checked": checked,
        })
      } else {
        self
          .evaluate_camoufox_element_action(
            &page,
            None,
            args.locator.as_ref(),
            &format!(
              r#"
                const desiredChecked = {};
                const isCheckboxInput = element instanceof HTMLInputElement && (element.type === "checkbox" || element.type === "radio");
                if (!isCheckboxInput) {{
                  return {{ ok: false, error: "Target is not a checkbox or radio input", target }};
                }}
                if (element.type === "radio" && desiredChecked === false) {{
                  return {{ ok: false, error: "Cannot uncheck a radio input directly", target }};
                }}
                const before = !!element.checked;
                if (before !== desiredChecked) {{
                  element.click();
                }}
                return {{
                  ok: true,
                  target,
                  changed: before !== !!element.checked,
                  checked: !!element.checked,
                }};
              "#,
              if desired_checked { "true" } else { "false" }
            ),
          )
          .await?
      };

      if payload.get("ok").and_then(|value| value.as_bool()) != Some(true) {
        let message = payload
          .get("error")
          .and_then(|value| value.as_str())
          .unwrap_or("Checkbox toggle failed");
        return Err(McpError::internal(message));
      }

      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "frame": serde_json::Value::Null,
        "resolvedFrame": serde_json::Value::Null,
        "timeoutMs": timeout_ms,
        "autoWait": readiness,
        "target": payload.get("target").cloned().unwrap_or_else(|| Self::camoufox_target_payload(args.selector.as_deref(), args.locator.as_ref())),
        "changed": payload.get("changed").cloned().unwrap_or(serde_json::Value::Bool(true)),
        "checked": payload.get("checked").cloned().unwrap_or(serde_json::Value::Bool(desired_checked)),
      }));
    }
    let desired_literal = if desired_checked { "true" } else { "false" };
    let js = Self::build_element_target_expression(
      args.selector.as_deref(),
      args.locator.as_ref(),
      &format!(
        r#"
        const desiredChecked = {desired_literal};
        const isCheckboxInput = element instanceof HTMLInputElement && (element.type === "checkbox" || element.type === "radio");
        if (!isCheckboxInput) {{
          return {{ ok: false, error: "Target is not a checkbox or radio input", target }};
        }}
        if (element.type === "radio" && desiredChecked === false) {{
          return {{ ok: false, error: "Cannot uncheck a radio input directly", target }};
        }}
        const before = !!element.checked;
        if (before !== desiredChecked) {{
          element.click();
        }}
        return {{
          ok: true,
          target,
          changed: before !== !!element.checked,
          checked: !!element.checked,
        }};
      "#
      ),
    )?;
    let (frame_selector, resolved_frame, readiness, result) = self
      .run_element_interaction(
        &args.profile_id,
        args.selector.as_deref(),
        args.locator.as_ref(),
        args.frame.as_ref(),
        timeout_ms,
        auto_wait_enabled,
        "click",
        js,
        false,
      )
      .await?;
    let payload = result
      .get("result")
      .and_then(|value| value.get("value"))
      .cloned()
      .unwrap_or(serde_json::Value::Null);
    if payload.get("ok").and_then(|value| value.as_bool()) != Some(true) {
      let message = payload
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or("Checkbox toggle failed");
      return Err(McpError::internal(message));
    }
    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "frame": frame_selector,
      "resolvedFrame": resolved_frame,
      "timeoutMs": timeout_ms,
      "autoWait": readiness,
      "target": payload.get("target").cloned().unwrap_or(serde_json::Value::Null),
      "changed": payload.get("changed").cloned().unwrap_or(serde_json::Value::Null),
      "checked": payload.get("checked").cloned().unwrap_or(serde_json::Value::Null),
    }))
  }

  async fn handle_element_exists(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: LocatorReadArgs = Self::parse_arguments(arguments)?;
    args.locator.validate()?;
    let auto_wait_enabled = args.options.auto_wait.unwrap_or(true);
    let timeout_ms = args.options.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      Self::camoufox_not_supported_for_frames(args.frame.as_ref(), "element_exists")?;
      let page = self.get_camoufox_active_page(&profile).await?;
      let (snapshot, timed_out) = if auto_wait_enabled {
        self
          .wait_for_camoufox_locator_snapshot(&page, &args.locator, timeout_ms)
          .await?
      } else {
        (
          Self::parse_locator_resolution_snapshot(
            self
              .evaluate_camoufox_page_value(
                &page,
                &Self::build_locator_resolution_expression(&args.locator)?,
              )
              .await?,
          )?,
          false,
        )
      };
      let locator = args.locator.clone();
      let first_match_text = snapshot.text.clone();
      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "frame": args.frame,
        "resolvedFrame": serde_json::Value::Null,
        "timeoutMs": timeout_ms,
        "autoWait": {
          "enabled": auto_wait_enabled,
          "satisfied": !timed_out && snapshot.exists,
          "timedOut": auto_wait_enabled && timed_out,
          "state": {
            "locator": locator,
            "exists": snapshot.exists,
            "visible": snapshot.visible,
            "count": snapshot.count,
            "firstMatchText": first_match_text,
          }
        },
        "locator": args.locator,
        "exists": snapshot.exists,
        "visible": snapshot.visible,
        "count": snapshot.count,
        "firstMatchText": snapshot.text,
      }));
    }
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    let (frame_selector, resolved_frame, context_id) = self
      .resolve_frame_context(&args.profile_id, &ws_url, args.frame.as_ref())
      .await?;
    let (snapshot, timed_out) = if auto_wait_enabled {
      self
        .wait_for_locator_snapshot(&ws_url, &args.locator, context_id, timeout_ms)
        .await?
    } else {
      (
        self
          .evaluate_locator_snapshot_in_context(&ws_url, &args.locator, context_id)
          .await?,
        false,
      )
    };
    let locator = args.locator.clone();
    let first_match_text = snapshot.text.clone();
    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "frame": frame_selector,
      "resolvedFrame": resolved_frame,
      "timeoutMs": timeout_ms,
      "autoWait": {
        "enabled": auto_wait_enabled,
        "satisfied": !timed_out && snapshot.exists,
        "timedOut": auto_wait_enabled && timed_out,
        "state": {
          "locator": locator,
          "exists": snapshot.exists,
          "visible": snapshot.visible,
          "count": snapshot.count,
          "firstMatchText": first_match_text,
        }
      },
      "locator": args.locator,
      "exists": snapshot.exists,
      "visible": snapshot.visible,
      "count": snapshot.count,
      "firstMatchText": snapshot.text,
    }))
  }

  async fn handle_get_element_text(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: LocatorReadArgs = Self::parse_arguments(arguments)?;
    args.locator.validate()?;
    let auto_wait_enabled = args.options.auto_wait.unwrap_or(true);
    let timeout_ms = args.options.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      Self::camoufox_not_supported_for_frames(args.frame.as_ref(), "get_element_text")?;
      let page = self.get_camoufox_active_page(&profile).await?;
      let (snapshot, timed_out) = if auto_wait_enabled {
        self
          .wait_for_camoufox_locator_snapshot(&page, &args.locator, timeout_ms)
          .await?
      } else {
        (
          Self::parse_locator_resolution_snapshot(
            self
              .evaluate_camoufox_page_value(
                &page,
                &Self::build_locator_resolution_expression(&args.locator)?,
              )
              .await?,
          )?,
          false,
        )
      };
      let locator = args.locator.clone();
      let first_match_text = snapshot.text.clone();
      if !snapshot.exists {
        if timed_out {
          return Err(McpError::timeout(format!(
            "Timed out after {timeout_ms}ms waiting for text target {:?}. Last observed state: {}",
            args.locator,
            serde_json::to_string(&serde_json::json!({
              "locator": locator,
              "exists": snapshot.exists,
              "visible": snapshot.visible,
              "count": snapshot.count,
              "firstMatchText": first_match_text,
            }))
            .unwrap_or_else(|_| "null".to_string())
          )));
        }
        return Err(McpError::not_found(format!(
          "No element found for locator {:?}",
          args.locator
        )));
      }
      Self::ensure_single_target_locator(&args.locator, &snapshot, "get_element_text")?;
      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "frame": args.frame,
        "resolvedFrame": serde_json::Value::Null,
        "timeoutMs": timeout_ms,
        "autoWait": {
          "enabled": auto_wait_enabled,
          "satisfied": true,
          "timedOut": false,
          "state": {
            "locator": locator,
            "exists": snapshot.exists,
            "visible": snapshot.visible,
            "count": snapshot.count,
            "firstMatchText": first_match_text,
          }
        },
        "locator": args.locator,
        "text": snapshot.text,
        "visible": snapshot.visible,
        "count": snapshot.count,
      }));
    }
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    let (frame_selector, resolved_frame, context_id) = self
      .resolve_frame_context(&args.profile_id, &ws_url, args.frame.as_ref())
      .await?;
    let (snapshot, timed_out) = if auto_wait_enabled {
      self
        .wait_for_locator_snapshot(&ws_url, &args.locator, context_id, timeout_ms)
        .await?
    } else {
      (
        self
          .evaluate_locator_snapshot_in_context(&ws_url, &args.locator, context_id)
          .await?,
        false,
      )
    };
    let locator = args.locator.clone();
    let first_match_text = snapshot.text.clone();
    if !snapshot.exists {
      if timed_out {
        return Err(McpError::timeout(format!(
          "Timed out after {timeout_ms}ms waiting for text target {:?}. Last observed state: {}",
          args.locator,
          serde_json::to_string(&serde_json::json!({
            "locator": locator,
            "exists": snapshot.exists,
            "visible": snapshot.visible,
            "count": snapshot.count,
            "firstMatchText": first_match_text,
          }))
          .unwrap_or_else(|_| "null".to_string())
        )));
      }
      return Err(McpError::not_found(format!(
        "No element found for locator {:?}",
        args.locator
      )));
    }
    Self::ensure_single_target_locator(&args.locator, &snapshot, "get_element_text")?;

    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "frame": frame_selector,
      "resolvedFrame": resolved_frame,
      "timeoutMs": timeout_ms,
      "autoWait": {
        "enabled": auto_wait_enabled,
        "satisfied": true,
        "timedOut": false,
        "state": {
          "locator": locator,
          "exists": snapshot.exists,
          "visible": snapshot.visible,
          "count": snapshot.count,
          "firstMatchText": first_match_text,
        }
      },
      "locator": args.locator,
      "text": snapshot.text,
      "visible": snapshot.visible,
      "count": snapshot.count,
    }))
  }

  async fn handle_get_element_html(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: FrameLocatorReadArgs = Self::parse_arguments(arguments)?;
    let auto_wait_enabled = args.options.auto_wait.unwrap_or(true);
    let timeout_ms = args.options.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
    let (ws_url, frame_selector, resolved_frame, context_id, snapshot, timed_out) = self
      .prepare_locator_read_context(
        &args.profile_id,
        &args.locator,
        args.frame.as_ref(),
        auto_wait_enabled,
        timeout_ms,
      )
      .await?;
    if !snapshot.exists {
      if timed_out {
        return Err(McpError::timeout(format!(
          "Timed out after {timeout_ms}ms waiting for HTML target {:?}. Last observed state: {}",
          args.locator,
          serde_json::to_string(&serde_json::json!({
            "locator": args.locator,
            "exists": snapshot.exists,
            "visible": snapshot.visible,
            "count": snapshot.count,
            "firstMatchText": snapshot.text,
          }))
          .unwrap_or_else(|_| "null".to_string())
        )));
      }
      return Err(McpError::not_found(format!(
        "No element found for locator {:?}",
        args.locator
      )));
    }
    Self::ensure_single_target_locator(&args.locator, &snapshot, "get_element_html")?;
    let expression = Self::build_element_target_expression(
      None,
      Some(&args.locator),
      r#"
        return {
          ok: true,
          target,
          html: element.outerHTML,
        };
      "#,
    )?;
    let result = self
      .execute_runtime_expression(&ws_url, &expression, context_id, false)
      .await?;
    let payload = Self::runtime_result_value(&result);
    if payload.get("ok").and_then(|value| value.as_bool()) != Some(true) {
      let message = payload
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or("Get element HTML failed");
      return Err(McpError::internal(message));
    }
    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "frame": frame_selector,
      "resolvedFrame": resolved_frame,
      "timeoutMs": timeout_ms,
      "autoWait": {
        "enabled": auto_wait_enabled,
        "satisfied": true,
        "timedOut": false,
        "state": {
          "locator": args.locator,
          "exists": snapshot.exists,
          "visible": snapshot.visible,
          "count": snapshot.count,
          "firstMatchText": snapshot.text,
        }
      },
      "locator": args.locator,
      "html": payload.get("html").cloned().unwrap_or(serde_json::Value::Null),
    }))
  }

  async fn handle_get_element_attribute(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: GetElementAttributeArgs = Self::parse_arguments(arguments)?;
    Self::validate_non_empty_key(&args.name, "name")?;
    let auto_wait_enabled = args.options.auto_wait.unwrap_or(true);
    let timeout_ms = args.options.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
    let (ws_url, frame_selector, resolved_frame, context_id, snapshot, timed_out) = self
      .prepare_locator_read_context(
        &args.profile_id,
        &args.locator,
        args.frame.as_ref(),
        auto_wait_enabled,
        timeout_ms,
      )
      .await?;
    if !snapshot.exists {
      if timed_out {
        return Err(McpError::timeout(format!(
          "Timed out after {timeout_ms}ms waiting for attribute target {:?}. Last observed state: {}",
          args.locator,
          serde_json::to_string(&serde_json::json!({
            "locator": args.locator,
            "exists": snapshot.exists,
            "visible": snapshot.visible,
            "count": snapshot.count,
            "firstMatchText": snapshot.text,
          }))
          .unwrap_or_else(|_| "null".to_string())
        )));
      }
      return Err(McpError::not_found(format!(
        "No element found for locator {:?}",
        args.locator
      )));
    }
    Self::ensure_single_target_locator(&args.locator, &snapshot, "get_element_attribute")?;
    let name_literal = serde_json::to_string(&args.name)
      .map_err(|e| McpError::internal(format!("Failed to serialize attribute name: {e}")))?;
    let expression = Self::build_element_target_expression(
      None,
      Some(&args.locator),
      &format!(
        r#"
        const attributeName = {name_literal};
        return {{
          ok: true,
          target,
          name: attributeName,
          value: element.getAttribute(attributeName),
        }};
      "#
      ),
    )?;
    let result = self
      .execute_runtime_expression(&ws_url, &expression, context_id, false)
      .await?;
    let payload = Self::runtime_result_value(&result);
    if payload.get("ok").and_then(|value| value.as_bool()) != Some(true) {
      let message = payload
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or("Get element attribute failed");
      return Err(McpError::internal(message));
    }
    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "frame": frame_selector,
      "resolvedFrame": resolved_frame,
      "timeoutMs": timeout_ms,
      "autoWait": {
        "enabled": auto_wait_enabled,
        "satisfied": true,
        "timedOut": false,
        "state": {
          "locator": args.locator,
          "exists": snapshot.exists,
          "visible": snapshot.visible,
          "count": snapshot.count,
          "firstMatchText": snapshot.text,
        }
      },
      "locator": args.locator,
      "name": args.name,
      "value": payload.get("value").cloned().unwrap_or(serde_json::Value::Null),
    }))
  }

  async fn handle_get_element_rect(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: FrameLocatorReadArgs = Self::parse_arguments(arguments)?;
    let auto_wait_enabled = args.options.auto_wait.unwrap_or(true);
    let timeout_ms = args.options.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
    let (ws_url, frame_selector, resolved_frame, context_id, snapshot, timed_out) = self
      .prepare_locator_read_context(
        &args.profile_id,
        &args.locator,
        args.frame.as_ref(),
        auto_wait_enabled,
        timeout_ms,
      )
      .await?;
    if !snapshot.exists {
      if timed_out {
        return Err(McpError::timeout(format!(
          "Timed out after {timeout_ms}ms waiting for rect target {:?}. Last observed state: {}",
          args.locator,
          serde_json::to_string(&serde_json::json!({
            "locator": args.locator,
            "exists": snapshot.exists,
            "visible": snapshot.visible,
            "count": snapshot.count,
            "firstMatchText": snapshot.text,
          }))
          .unwrap_or_else(|_| "null".to_string())
        )));
      }
      return Err(McpError::not_found(format!(
        "No element found for locator {:?}",
        args.locator
      )));
    }
    Self::ensure_single_target_locator(&args.locator, &snapshot, "get_element_rect")?;
    let expression = Self::build_element_target_expression(
      None,
      Some(&args.locator),
      r#"
        const rect = element.getBoundingClientRect();
        return {
          ok: true,
          target,
          rect: {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            top: rect.top,
            right: rect.right,
            bottom: rect.bottom,
            left: rect.left,
          },
        };
      "#,
    )?;
    let result = self
      .execute_runtime_expression(&ws_url, &expression, context_id, false)
      .await?;
    let payload = Self::runtime_result_value(&result);
    if payload.get("ok").and_then(|value| value.as_bool()) != Some(true) {
      let message = payload
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or("Get element rect failed");
      return Err(McpError::internal(message));
    }
    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "frame": frame_selector,
      "resolvedFrame": resolved_frame,
      "timeoutMs": timeout_ms,
      "autoWait": {
        "enabled": auto_wait_enabled,
        "satisfied": true,
        "timedOut": false,
        "state": {
          "locator": args.locator,
          "exists": snapshot.exists,
          "visible": snapshot.visible,
          "count": snapshot.count,
          "firstMatchText": snapshot.text,
        }
      },
      "locator": args.locator,
      "rect": payload.get("rect").cloned().unwrap_or(serde_json::Value::Null),
    }))
  }

  async fn handle_query_elements(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: QueryElementsArgs = Self::parse_arguments(arguments)?;
    args.locator.validate()?;
    let limit = args.limit.unwrap_or(20).min(100);
    let auto_wait_enabled = args.options.auto_wait.unwrap_or(true);
    let timeout_ms = args.options.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      Self::camoufox_not_supported_for_frames(args.frame.as_ref(), "query_elements")?;
      let page = self.get_camoufox_active_page(&profile).await?;
      let (value, timed_out) = if auto_wait_enabled {
        self
          .wait_for_camoufox_locator_collection(&page, &args.locator, limit, timeout_ms)
          .await?
      } else {
        (
          self
            .evaluate_camoufox_page_value(
              &page,
              &Self::build_locator_collection_expression(&args.locator, limit)?,
            )
            .await?,
          false,
        )
      };
      let count = value.get("count").cloned().unwrap_or(serde_json::json!(0));
      let elements = value
        .get("elements")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));
      let returned = elements
        .as_array()
        .map(|elements| elements.len())
        .unwrap_or(0);
      let locator = args.locator.clone();
      let count_for_wait = count.clone();
      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "frame": args.frame,
        "resolvedFrame": serde_json::Value::Null,
        "timeoutMs": timeout_ms,
        "autoWait": {
          "enabled": auto_wait_enabled,
          "satisfied": !timed_out && count.as_u64().unwrap_or(0) > 0,
          "timedOut": auto_wait_enabled && timed_out,
          "state": {
            "locator": locator,
            "count": count_for_wait,
            "returned": returned,
          }
        },
        "locator": args.locator,
        "count": count,
        "returned": returned,
        "limit": limit,
        "elements": elements,
      }));
    }
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    let (frame_selector, resolved_frame, context_id) = self
      .resolve_frame_context(&args.profile_id, &ws_url, args.frame.as_ref())
      .await?;
    let (value, timed_out) = if auto_wait_enabled {
      self
        .wait_for_locator_collection(&ws_url, &args.locator, limit, context_id, timeout_ms)
        .await?
    } else {
      (
        self
          .evaluate_runtime_value(
            &ws_url,
            &Self::build_locator_collection_expression(&args.locator, limit)?,
            context_id,
          )
          .await?,
        false,
      )
    };
    let count = value.get("count").cloned().unwrap_or(serde_json::json!(0));
    let elements = value
      .get("elements")
      .cloned()
      .unwrap_or_else(|| serde_json::json!([]));
    let returned = elements
      .as_array()
      .map(|elements| elements.len())
      .unwrap_or(0);
    let locator = args.locator.clone();
    let count_for_wait = count.clone();

    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "frame": frame_selector,
      "resolvedFrame": resolved_frame,
      "timeoutMs": timeout_ms,
      "autoWait": {
        "enabled": auto_wait_enabled,
        "satisfied": !timed_out && count.as_u64().unwrap_or(0) > 0,
        "timedOut": auto_wait_enabled && timed_out,
        "state": {
          "locator": locator,
          "count": count_for_wait,
          "returned": returned,
        }
      },
      "locator": args.locator,
      "count": count,
      "returned": returned,
      "limit": limit,
      "elements": elements,
    }))
  }

  async fn handle_count_elements(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: LocatorReadArgs = Self::parse_arguments(arguments)?;
    let expression = Self::build_locator_collection_expression(&args.locator, 0)?;
    let value = self
      .evaluate_javascript_value(&args.profile_id, &expression)
      .await?;

    if let Some(error) = value.get("error").and_then(|value| value.as_str()) {
      return Err(McpError::invalid_params(error));
    }

    let count = value.get("count").cloned().unwrap_or(serde_json::json!(0));
    let exists = count.as_u64().unwrap_or(0) > 0;
    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "locator": args.locator,
      "count": count,
      "exists": exists,
    }))
  }

  async fn handle_get_page_content(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: GetPageContentArgs = Self::parse_arguments(arguments)?;
    if let Some(frame) = args.frame.as_ref() {
      frame.validate()?;
    }
    let format = args.format.as_deref().unwrap_or("text");
    let selector = args.selector.as_deref();

    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      Self::camoufox_not_supported_for_frames(args.frame.as_ref(), "get_page_content")?;
      let page = self.get_camoufox_active_page(&profile).await?;
      let content = if let Some(selector) = selector {
        let selector_literal = serde_json::to_string(selector)
          .map_err(|error| McpError::internal(format!("Failed to serialize selector: {error}")))?;
        let expression = if format == "html" {
          format!(
            r#"(() => {{
              const element = document.querySelector({selector_literal});
              return element ? element.outerHTML : null;
            }})()"#
          )
        } else {
          format!(
            r#"(() => {{
              const element = document.querySelector({selector_literal});
              return element ? String(element.innerText || element.textContent || "") : null;
            }})()"#
          )
        };
        self
          .evaluate_camoufox_page_value(&page, &expression)
          .await?
      } else if format == "html" {
        self
          .evaluate_camoufox_page_value(&page, "document.documentElement.outerHTML")
          .await?
      } else {
        self
          .evaluate_camoufox_page_value(&page, "document.body ? document.body.innerText : ''")
          .await?
      };

      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "frame": args.frame,
        "resolvedFrame": serde_json::Value::Null,
        "format": format,
        "selector": args.selector,
        "content": content,
      }));
    }
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    let (frame_selector, resolved_frame, context_id) = self
      .resolve_frame_context(&args.profile_id, &ws_url, args.frame.as_ref())
      .await?;

    let js = if let Some(sel) = selector {
      let sel_escaped = sel.replace('\\', "\\\\").replace('\'', "\\'");
      if format == "html" {
        format!(
          r#"(() => {{
            const el = document.querySelector('{}');
            return el ? el.outerHTML : null;
          }})()"#,
          sel_escaped
        )
      } else {
        format!(
          r#"(() => {{
            const el = document.querySelector('{}');
            return el ? el.innerText : null;
          }})()"#,
          sel_escaped
        )
      }
    } else if format == "html" {
      "document.documentElement.outerHTML".to_string()
    } else {
      "document.body.innerText".to_string()
    };

    let content = self
      .evaluate_runtime_value(&ws_url, &js, context_id)
      .await?;

    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "frame": frame_selector,
      "resolvedFrame": resolved_frame,
      "format": format,
      "selector": args.selector,
      "content": content,
    }))
  }

  async fn handle_get_page_info(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = arguments
      .get("profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing profile_id".to_string(),
      })?;

    let profile = self.get_running_profile(profile_id)?;
    if profile.browser == "camoufox" {
      let page = self.get_camoufox_active_page(&profile).await?;
      let info = serde_json::json!({
        "url": page.url().map_err(|error| Self::camoufox_error("page URL lookup", error))?,
        "title": page.title().await.map_err(|error| Self::camoufox_error("page title lookup", error))?,
        "readyState": self
          .evaluate_camoufox_page_value(&page, "document.readyState")
          .await?,
      });

      return Ok(serde_json::json!({
        "content": [{
          "type": "text",
          "text": serde_json::to_string_pretty(&info).unwrap_or_default()
        }]
      }));
    }
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;

    let result = self
      .send_cdp(
        &ws_url,
        "Runtime.evaluate",
        serde_json::json!({
          "expression": "JSON.stringify({url: location.href, title: document.title, readyState: document.readyState})",
          "returnByValue": true,
        }),
      )
      .await?;

    let info_str = result
      .get("result")
      .and_then(|r| r.get("value"))
      .and_then(|v| v.as_str())
      .unwrap_or("{}");

    let info: serde_json::Value = serde_json::from_str(info_str).unwrap_or(serde_json::json!({}));

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&info).unwrap_or_default()
      }]
    }))
  }

  async fn handle_list_tabs(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: TabTreeArgs = Self::parse_arguments(arguments)?;
    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      let (pages, active_index) = self.get_camoufox_pages(&profile).await?;
      let tabs = self.build_camoufox_tab_nodes(pages, active_index).await?;
      let active_tab_id = tabs.iter().find(|tab| tab.active).map(|tab| tab.id.clone());

      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "total": tabs.len(),
        "activeTabId": active_tab_id,
        "tabs": tabs,
      }));
    }
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let page_targets = self.get_cdp_page_targets(cdp_port).await?;
    let active_tab_id = self.resolve_active_tab_id(cdp_port, &page_targets).await;
    let tabs = Self::build_tab_nodes(&page_targets, active_tab_id.as_deref());

    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "total": tabs.len(),
      "activeTabId": active_tab_id,
      "tabs": tabs,
    }))
  }

  async fn handle_get_active_tab(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: TabTreeArgs = Self::parse_arguments(arguments)?;
    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      let (pages, active_index) = self.get_camoufox_pages(&profile).await?;
      let tabs = self.build_camoufox_tab_nodes(pages, active_index).await?;
      let active_tab = tabs
        .into_iter()
        .find(|tab| tab.active)
        .ok_or_else(|| McpError::not_found("No tab is currently available"))?;

      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "tab": active_tab,
      }));
    }
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let page_targets = self.get_cdp_page_targets(cdp_port).await?;
    let active_tab_id = self
      .resolve_active_tab_id(cdp_port, &page_targets)
      .await
      .ok_or_else(|| McpError::not_found("No tab is currently available"))?;
    let tabs = Self::build_tab_nodes(&page_targets, Some(active_tab_id.as_str()));
    let active_tab = tabs
      .into_iter()
      .find(|tab| tab.id == active_tab_id)
      .ok_or_else(|| McpError::internal("Active tab disappeared during resolution"))?;

    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "tab": active_tab,
    }))
  }

  async fn handle_switch_tab(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: SwitchTabArgs = Self::parse_arguments(arguments)?;
    args.tab.validate()?;

    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      let selector_value_owned = match &args.tab.value {
        Some(McpTabValue::Text(value)) => Some(value.clone()),
        Some(McpTabValue::Index(index)) => Some(index.to_string()),
        None => None,
      };
      let (pages, active_index) = crate::camoufox_manager::CamoufoxManager::instance()
        .set_selected_tab(
          &mcp_profile_path(&profile),
          args.tab.by.as_str(),
          selector_value_owned.as_deref(),
        )
        .await
        .map_err(|error| Self::camoufox_error("tab switching", error))?;
      let tabs = self.build_camoufox_tab_nodes(pages, active_index).await?;
      let selected_tab = tabs
        .into_iter()
        .find(|tab| tab.active)
        .ok_or_else(|| McpError::internal("Active Camoufox tab disappeared during switch"))?;

      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "tab": selected_tab,
      }));
    }
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let page_targets = self.get_cdp_page_targets(cdp_port).await?;
    let active_tab_id = self.resolve_active_tab_id(cdp_port, &page_targets).await;
    let tabs = Self::build_tab_nodes(&page_targets, active_tab_id.as_deref());
    let selected_tab = Self::resolve_tab_selector(&tabs, &args.tab)?;

    self.activate_tab_target(cdp_port, &selected_tab.id).await?;

    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "tab": McpTabNode {
        active: true,
        ..selected_tab
      },
    }))
  }

  async fn handle_new_tab(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: NewTabArgs = Self::parse_arguments(arguments)?;
    if matches!(args.url.as_deref(), Some(url) if url.trim().is_empty()) {
      return Err(McpError::invalid_params("url cannot be empty"));
    }

    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      let requested_url = args.url.unwrap_or_else(|| "about:blank".to_string());
      let (pages, active_index) = crate::camoufox_manager::CamoufoxManager::instance()
        .new_tab(&mcp_profile_path(&profile), Some(requested_url.as_str()))
        .await
        .map_err(|error| Self::camoufox_error("new tab creation", error))?;
      let tabs = self.build_camoufox_tab_nodes(pages, active_index).await?;
      let tab = tabs
        .iter()
        .find(|tab| tab.active)
        .cloned()
        .ok_or_else(|| McpError::internal("New Camoufox tab disappeared after creation"))?;

      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "requestedUrl": requested_url,
        "tab": tab,
      }));
    }
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let requested_url = args.url.unwrap_or_else(|| "about:blank".to_string());
    let created_tab = self.create_tab_target(cdp_port, &requested_url).await?;
    self
      .set_selected_tab_for_port(cdp_port, Some(created_tab.id.clone()))
      .await;

    let page_targets = self.get_cdp_page_targets(cdp_port).await?;
    let active_tab_id = self
      .resolve_active_tab_id(cdp_port, &page_targets)
      .await
      .unwrap_or_else(|| created_tab.id.clone());
    let tabs = Self::build_tab_nodes(&page_targets, Some(active_tab_id.as_str()));
    let tab = tabs
      .into_iter()
      .find(|tab| tab.id == created_tab.id)
      .unwrap_or(McpTabNode {
        id: created_tab.id.clone(),
        index: page_targets.len().saturating_sub(1),
        title: created_tab.title.clone(),
        url: created_tab.url.clone(),
        target_type: created_tab.target_type.clone(),
        active: active_tab_id == created_tab.id,
      });

    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "requestedUrl": requested_url,
      "tab": tab,
    }))
  }

  async fn handle_close_tab(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: CloseTabArgs = Self::parse_arguments(arguments)?;
    if let Some(tab) = args.tab.as_ref() {
      tab.validate()?;
    }

    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      let (pages_before, active_before) = self.get_camoufox_pages(&profile).await?;
      let tabs_before = self
        .build_camoufox_tab_nodes(pages_before, active_before)
        .await?;
      let tab_to_close = if let Some(tab) = args.tab.as_ref() {
        Self::resolve_tab_selector(&tabs_before, tab)?
      } else {
        tabs_before
          .iter()
          .find(|tab| tab.active)
          .cloned()
          .ok_or_else(|| McpError::not_found("No active tab is currently available"))?
      };
      let selector_by = args.tab.as_ref().map(|tab| tab.by.as_str());
      let selector_value_owned = args.tab.as_ref().and_then(|tab| match &tab.value {
        Some(McpTabValue::Text(value)) => Some(value.clone()),
        Some(McpTabValue::Index(index)) => Some(index.to_string()),
        None => None,
      });
      let (remaining_pages, next_active_index) =
        crate::camoufox_manager::CamoufoxManager::instance()
          .close_tab(
            &mcp_profile_path(&profile),
            selector_by,
            selector_value_owned.as_deref(),
          )
          .await
          .map_err(|error| Self::camoufox_error("tab close", error))?;
      let active_index = next_active_index.unwrap_or(0);
      let tabs = if remaining_pages.is_empty() {
        Vec::new()
      } else {
        self
          .build_camoufox_tab_nodes(remaining_pages, active_index)
          .await?
      };
      let active_tab_id = tabs.iter().find(|tab| tab.active).map(|tab| tab.id.clone());

      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "closedTab": tab_to_close,
        "remaining": tabs.len(),
        "activeTabId": active_tab_id,
        "tabs": tabs,
      }));
    }
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let page_targets = self.get_cdp_page_targets(cdp_port).await?;
    let active_tab_id = self.resolve_active_tab_id(cdp_port, &page_targets).await;
    let tabs = Self::build_tab_nodes(&page_targets, active_tab_id.as_deref());
    let tab_to_close = if let Some(tab) = args.tab.as_ref() {
      Self::resolve_tab_selector(&tabs, tab)?
    } else {
      tabs
        .iter()
        .find(|tab| tab.active)
        .cloned()
        .ok_or_else(|| McpError::not_found("No active tab is currently available"))?
    };

    self.close_tab_target(cdp_port, &tab_to_close.id).await?;

    let remaining_targets = self.get_cdp_page_targets(cdp_port).await?;
    let next_active_tab_id = self
      .resolve_active_tab_id(cdp_port, &remaining_targets)
      .await;

    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "closedTab": tab_to_close,
      "remaining": remaining_targets.len(),
      "activeTabId": next_active_tab_id,
      "tabs": Self::build_tab_nodes(&remaining_targets, next_active_tab_id.as_deref()),
    }))
  }

  async fn handle_file_input_action(
    &self,
    args: FileInputArgs,
    action_name: &str,
  ) -> Result<serde_json::Value, McpError> {
    let resolved_paths = Self::validate_file_input_args(&args, action_name)?;
    let auto_wait_enabled = args.options.auto_wait.unwrap_or(true);
    let timeout_ms = args.options.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);

    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      Self::camoufox_not_supported_for_frames(args.frame.as_ref(), action_name)?;
      let page = self.get_camoufox_active_page(&profile).await?;
      let readiness_expression = Self::build_file_input_readiness_expression(
        args.selector.as_deref(),
        args.locator.as_ref(),
      )?;
      let readiness = if auto_wait_enabled {
        self
          .wait_with_timeout(
            timeout_ms,
            || {
              let page = page.clone();
              let readiness_expression = readiness_expression.clone();
              async move {
                let value = self
                  .evaluate_camoufox_page_value(&page, &readiness_expression)
                  .await?;
                if value.get("ok").and_then(|value| value.as_bool()) == Some(true) {
                  Ok(Some(value))
                } else {
                  Ok(None)
                }
              }
            },
            || format!("Timed out after {timeout_ms}ms waiting for file input to become ready"),
          )
          .await?
      } else {
        self
          .evaluate_camoufox_page_value(&page, &readiness_expression)
          .await?
      };

      if readiness.get("ok").and_then(|value| value.as_bool()) != Some(true) {
        return Err(McpError::invalid_params(
          readiness
            .get("error")
            .and_then(|value| value.as_str())
            .unwrap_or("File input is not ready"),
        ));
      }

      let files_payload = Self::build_dom_file_payloads(&resolved_paths)?;
      let files_payload_literal = serde_json::to_string(&files_payload).map_err(|error| {
        McpError::internal(format!("Failed to serialize upload files: {error}"))
      })?;
      let assign_expression = Self::build_element_target_expression(
        args.selector.as_deref(),
        args.locator.as_ref(),
        &format!(
          r#"
          const files = {files_payload_literal};
          const dataTransfer = new DataTransfer();
          for (const file of files) {{
            const binary = atob(file.base64);
            const bytes = Uint8Array.from(binary, (char) => char.charCodeAt(0));
            dataTransfer.items.add(new File([bytes], file.name, {{
              type: file.mime,
              lastModified: file.lastModified,
            }}));
          }}
          element.files = dataTransfer.files;
          element.dispatchEvent(new Event("input", {{ bubbles: true }}));
          element.dispatchEvent(new Event("change", {{ bubbles: true }}));
          return {{
            ok: true,
            target,
            count: element.files ? element.files.length : 0,
            multiple: !!element.multiple,
            files: Array.from(element.files || []).map((file) => ({{
              name: file.name,
              size: file.size,
              type: file.type,
            }})),
          }};
      "#
        ),
      )?;
      let snapshot = self
        .evaluate_camoufox_page_value(&page, &assign_expression)
        .await?;

      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "frame": serde_json::Value::Null,
        "resolvedFrame": serde_json::Value::Null,
        "selector": args.selector,
        "locator": args.locator,
        "files": resolved_paths
          .iter()
          .map(|path| path.to_string_lossy().to_string())
          .collect::<Vec<_>>(),
        "timeoutMs": timeout_ms,
        "autoWait": auto_wait_enabled,
        "readiness": readiness,
        "result": snapshot,
        "action": action_name,
      }));
    }
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    let (frame_selector, resolved_frame, context_id) = self
      .resolve_frame_context(&args.profile_id, &ws_url, args.frame.as_ref())
      .await?;

    let readiness_expression =
      Self::build_file_input_readiness_expression(args.selector.as_deref(), args.locator.as_ref())?;
    let readiness = if auto_wait_enabled {
      self
        .wait_with_timeout(
          timeout_ms,
          || {
            let ws_url = ws_url.clone();
            let readiness_expression = readiness_expression.clone();
            async move {
              let value = self
                .evaluate_runtime_value(&ws_url, &readiness_expression, context_id)
                .await?;
              if value.get("ok").and_then(|value| value.as_bool()) == Some(true) {
                Ok(Some(value))
              } else {
                Ok(None)
              }
            }
          },
          || format!("Timed out after {timeout_ms}ms waiting for file input to become ready"),
        )
        .await?
    } else {
      self
        .evaluate_runtime_value(&ws_url, &readiness_expression, context_id)
        .await?
    };

    if readiness.get("ok").and_then(|value| value.as_bool()) != Some(true) {
      return Err(McpError::invalid_params(
        readiness
          .get("error")
          .and_then(|value| value.as_str())
          .unwrap_or("File input is not ready"),
      ));
    }

    let handle_expression = Self::build_element_target_expression(
      args.selector.as_deref(),
      args.locator.as_ref(),
      r#"
          const isInput = element instanceof HTMLInputElement;
          const inputType = isInput ? String(element.getAttribute("type") || "").toLowerCase() : null;
          if (!isInput || inputType !== "file") {
            return null;
          }
          return element;
      "#,
    )?;
    let object_id = self
      .evaluate_runtime_object_id(&ws_url, &handle_expression, context_id)
      .await?
      .ok_or_else(|| McpError::not_found("Failed to resolve file input element handle"))?;

    let files = resolved_paths
      .iter()
      .map(|path| path.to_string_lossy().to_string())
      .collect::<Vec<_>>();
    let set_files_result = match self
      .send_cdp(
        &ws_url,
        "DOM.requestNode",
        serde_json::json!({
          "objectId": object_id.clone(),
        }),
      )
      .await
    {
      Ok(node_result) => {
        let node_id = node_result
          .get("nodeId")
          .and_then(|value| value.as_i64())
          .ok_or_else(|| McpError::internal("DOM.requestNode did not return a nodeId"))?;
        self
          .send_cdp(
            &ws_url,
            "DOM.setFileInputFiles",
            serde_json::json!({
              "nodeId": node_id,
              "files": files.clone(),
            }),
          )
          .await
      }
      Err(error) => {
        log::warn!(
          "[mcp] DOM.requestNode failed for {} on profile {}: {}",
          action_name,
          args.profile_id,
          error.message
        );
        self
          .send_cdp(
            &ws_url,
            "DOM.setFileInputFiles",
            serde_json::json!({
              "objectId": object_id.clone(),
              "files": files.clone(),
            }),
          )
          .await
      }
    };

    if let Err(error) = set_files_result {
      log::warn!(
        "[mcp] DOM.setFileInputFiles failed for {} on profile {}: {}. Falling back to DOM injection.",
        action_name,
        args.profile_id,
        error.message
      );
      let files_payload = Self::build_dom_file_payloads(&resolved_paths)?;
      let files_payload_literal = serde_json::to_string(&files_payload).map_err(|error| {
        McpError::internal(format!("Failed to serialize upload files: {error}"))
      })?;
      let assign_expression = Self::build_element_target_expression(
        args.selector.as_deref(),
        args.locator.as_ref(),
        &format!(
          r#"
          const files = {files_payload_literal};
          const dataTransfer = new DataTransfer();
          for (const file of files) {{
            const binary = atob(file.base64);
            const bytes = Uint8Array.from(binary, (char) => char.charCodeAt(0));
            dataTransfer.items.add(new File([bytes], file.name, {{
              type: file.mime,
              lastModified: file.lastModified,
            }}));
          }}
          element.files = dataTransfer.files;
          element.dispatchEvent(new Event("input", {{ bubbles: true }}));
          element.dispatchEvent(new Event("change", {{ bubbles: true }}));
          return {{
            ok: true,
            target,
            count: element.files ? element.files.length : 0,
            multiple: !!element.multiple,
            files: Array.from(element.files || []).map((file) => ({{
              name: file.name,
              size: file.size,
              type: file.type,
            }})),
          }};
      "#
        ),
      )?;
      let fallback_snapshot = self
        .evaluate_runtime_value(&ws_url, &assign_expression, context_id)
        .await?;
      if fallback_snapshot
        .get("ok")
        .and_then(|value| value.as_bool())
        != Some(true)
      {
        return Err(error);
      }
    }

    let snapshot_expression = Self::build_element_target_expression(
      args.selector.as_deref(),
      args.locator.as_ref(),
      r#"
          return {
            ok: true,
            count: element.files ? element.files.length : 0,
            multiple: !!element.multiple,
            files: Array.from(element.files || []).map((file) => ({
              name: file.name,
              size: file.size,
              type: file.type,
            })),
          };
      "#,
    )?;
    let snapshot = self
      .evaluate_runtime_value(&ws_url, &snapshot_expression, context_id)
      .await?;

    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "frame": frame_selector,
      "resolvedFrame": resolved_frame,
      "selector": args.selector,
      "locator": args.locator,
      "files": files,
      "timeoutMs": timeout_ms,
      "autoWait": auto_wait_enabled,
      "readiness": readiness,
      "result": snapshot,
      "action": action_name,
    }))
  }

  fn build_dom_file_payloads(
    resolved_paths: &[std::path::PathBuf],
  ) -> Result<Vec<serde_json::Value>, McpError> {
    resolved_paths
      .iter()
      .map(|path| {
        let body = std::fs::read(path).map_err(|error| {
          McpError::internal(format!(
            "Failed to read upload file {}: {error}",
            path.display()
          ))
        })?;
        let name = path
          .file_name()
          .and_then(|value| value.to_str())
          .unwrap_or("upload.bin")
          .to_string();
        let mime = mime_guess::from_path(path)
          .first_or_octet_stream()
          .essence_str()
          .to_string();
        let last_modified = std::fs::metadata(path)
          .and_then(|metadata| metadata.modified())
          .ok()
          .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
          .map(|duration| duration.as_millis() as u64)
          .unwrap_or(0);
        Ok(serde_json::json!({
          "name": name,
          "mime": mime,
          "base64": base64::engine::general_purpose::STANDARD.encode(body),
          "lastModified": last_modified,
        }))
      })
      .collect()
  }

  async fn handle_set_file_input(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: FileInputArgs = Self::parse_arguments(arguments)?;
    self.handle_file_input_action(args, "set_file_input").await
  }

  async fn handle_upload_file(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: FileInputArgs = Self::parse_arguments(arguments)?;
    self.handle_file_input_action(args, "upload_file").await
  }

  async fn handle_download_file(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: DownloadFileArgs = Self::parse_arguments(arguments)?;
    if matches!(args.url.as_deref(), Some(url) if url.trim().is_empty()) {
      return Err(McpError::invalid_params("url cannot be empty"));
    }
    if matches!(args.file_name.as_deref(), Some(file_name) if file_name.trim().is_empty()) {
      return Err(McpError::invalid_params("file_name cannot be empty"));
    }

    let profile = self.get_running_profile(&args.profile_id)?;
    let requested_url = if let Some(url) = args.url.clone() {
      url
    } else {
      let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
      let page_targets = self.get_cdp_page_targets(cdp_port).await?;
      let active_tab_id = self
        .resolve_active_tab_id(cdp_port, &page_targets)
        .await
        .ok_or_else(|| McpError::not_found("No active tab is currently available"))?;
      page_targets
        .into_iter()
        .find(|target| target.id == active_tab_id)
        .map(|target| target.url)
        .filter(|url| !url.trim().is_empty())
        .ok_or_else(|| McpError::not_found("Active tab does not have a downloadable URL"))?
    };

    let parsed_url = reqwest::Url::parse(&requested_url)
      .map_err(|e| McpError::invalid_params(format!("Invalid download URL: {e}")))?;
    let download_dir = self.get_profile_download_dir(&profile);
    std::fs::create_dir_all(&download_dir).map_err(|e| {
      McpError::internal(format!(
        "Failed to create profile download directory {}: {e}",
        download_dir.display()
      ))
    })?;

    let file_name = Self::pick_download_file_name(&parsed_url, args.file_name.as_deref());
    let file_path = Self::unique_download_path(&download_dir, &file_name);
    let cookie_header = self.build_cookie_header(&args.profile_id, &parsed_url)?;
    let download_id = Uuid::new_v4().to_string();
    let initial_record = DownloadRecord {
      download_id: download_id.clone(),
      profile_id: args.profile_id.clone(),
      url: requested_url.clone(),
      file_name: file_name.clone(),
      file_path: file_path.to_string_lossy().to_string(),
      status: "started".to_string(),
      bytes_written: None,
      error: None,
    };
    let state = self.insert_download_state(initial_record.clone()).await;

    tokio::spawn(async move {
      let client = reqwest::Client::new();
      let mut request = client.get(parsed_url.clone());
      if let Some(cookie_header) = cookie_header.clone() {
        request = request.header(reqwest::header::COOKIE, cookie_header);
      }

      match request.send().await {
        Ok(response) => {
          if !response.status().is_success() {
            let mut record = state.record.lock().await;
            record.status = "error".to_string();
            record.error = Some(format!("HTTP {}", response.status()));
            return;
          }

          match response.bytes().await {
            Ok(bytes) => match tokio::fs::write(&file_path, &bytes).await {
              Ok(()) => {
                let mut record = state.record.lock().await;
                record.status = "completed".to_string();
                record.bytes_written = Some(bytes.len() as u64);
              }
              Err(error) => {
                let mut record = state.record.lock().await;
                record.status = "error".to_string();
                record.error = Some(format!("Failed to write downloaded file: {error}"));
              }
            },
            Err(error) => {
              let mut record = state.record.lock().await;
              record.status = "error".to_string();
              record.error = Some(format!("Failed to read download body: {error}"));
            }
          }
        }
        Err(error) => {
          let mut record = state.record.lock().await;
          record.status = "error".to_string();
          record.error = Some(format!("Failed to download file: {error}"));
        }
      }
    });

    Self::json_tool_result(&initial_record)
  }

  async fn handle_wait_for_download(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: WaitForDownloadArgs = Self::parse_arguments(arguments)?;
    Self::validate_non_empty_key(&args.download_id, "download_id")?;
    let timeout_ms = args.options.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);

    let record = self
      .wait_with_timeout(
        timeout_ms,
        || {
          let download_id = args.download_id.clone();
          let profile_id = args.profile_id.clone();
          async move {
            let record = self.get_download_record(&download_id).await?;
            if record.profile_id != profile_id {
              return Err(McpError::not_found(format!(
                "Download {} does not belong to profile {}",
                download_id, profile_id
              )));
            }
            match record.status.as_str() {
              "completed" | "error" => Ok(Some(record)),
              _ => Ok(None),
            }
          }
        },
        || {
          format!(
            "Timed out after {timeout_ms}ms waiting for download {}",
            args.download_id
          )
        },
      )
      .await?;

    Self::json_tool_result(&record)
  }

  async fn fetch_current_frame_tree(&self, profile_id: &str) -> Result<McpFrameTreeNode, McpError> {
    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    let result = self
      .send_cdp(&ws_url, "Page.getFrameTree", serde_json::json!({}))
      .await?;
    Self::parse_cdp_frame_tree(&result)
  }

  async fn resolve_frame_for_profile(
    &self,
    profile_id: &str,
    selector: &McpFrameRef,
  ) -> Result<McpFrameNode, McpError> {
    let root = self.fetch_current_frame_tree(profile_id).await?;
    let frames = Self::flatten_frame_tree(&root);
    Self::resolve_frame_selector(&frames, selector)
  }

  async fn create_isolated_world_context_id(
    &self,
    ws_url: &str,
    frame_id: &str,
  ) -> Result<i64, McpError> {
    let result = self
      .send_cdp(
        ws_url,
        "Page.createIsolatedWorld",
        serde_json::json!({
          "frameId": frame_id,
          "worldName": "jnmBrowserMcp",
          "grantUniveralAccess": true,
        }),
      )
      .await?;

    Self::parse_execution_context_id(&result)
  }

  async fn resolve_frame_context(
    &self,
    profile_id: &str,
    ws_url: &str,
    frame: Option<&McpFrameRef>,
  ) -> Result<(Option<McpFrameRef>, Option<McpFrameNode>, Option<i64>), McpError> {
    if let Some(frame) = frame {
      frame.validate()?;
      let resolved_frame = self.resolve_frame_for_profile(profile_id, frame).await?;
      let context_id = self
        .create_isolated_world_context_id(ws_url, &resolved_frame.id)
        .await?;
      Ok((Some(frame.clone()), Some(resolved_frame), Some(context_id)))
    } else {
      Ok((None, None, None))
    }
  }

  async fn prepare_locator_read_context(
    &self,
    profile_id: &str,
    locator: &McpLocator,
    frame: Option<&McpFrameRef>,
    auto_wait_enabled: bool,
    timeout_ms: u64,
  ) -> Result<
    (
      String,
      Option<McpFrameRef>,
      Option<McpFrameNode>,
      Option<i64>,
      LocatorResolutionSnapshot,
      bool,
    ),
    McpError,
  > {
    locator.validate()?;
    if let Some(frame) = frame {
      frame.validate()?;
    }

    let profile = self.get_running_profile(profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    let (frame_selector, resolved_frame, context_id) = self
      .resolve_frame_context(profile_id, &ws_url, frame)
      .await?;
    let (snapshot, timed_out) = if auto_wait_enabled {
      self
        .wait_for_locator_snapshot(&ws_url, locator, context_id, timeout_ms)
        .await?
    } else {
      (
        self
          .evaluate_locator_snapshot_in_context(&ws_url, locator, context_id)
          .await?,
        false,
      )
    };

    Ok((
      ws_url,
      frame_selector,
      resolved_frame,
      context_id,
      snapshot,
      timed_out,
    ))
  }

  async fn handle_list_frames(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: FrameTreeArgs = Self::parse_arguments(arguments)?;
    let root = self.fetch_current_frame_tree(&args.profile_id).await?;
    let frames = Self::flatten_frame_tree(&root);

    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "total": frames.len(),
      "frames": frames,
    }))
  }

  async fn handle_get_frame_tree(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: FrameTreeArgs = Self::parse_arguments(arguments)?;
    let root = self.fetch_current_frame_tree(&args.profile_id).await?;
    let total = Self::flatten_frame_tree(&root).len();

    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "total": total,
      "root": root,
    }))
  }

  async fn handle_find_frame(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: FindFrameArgs = Self::parse_arguments(arguments)?;
    let root = self.fetch_current_frame_tree(&args.profile_id).await?;
    let frames = Self::flatten_frame_tree(&root);
    let resolved = Self::resolve_frame_selector(&frames, &args.frame)?;

    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "frame": args.frame,
      "resolvedFrame": resolved,
    }))
  }

  async fn handle_start_console_capture(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: StartConsoleCaptureArgs = Self::parse_arguments(arguments)?;
    let profile = self.get_running_profile(&args.profile_id)?;
    let timeout_ms = args
      .options
      .timeout_ms
      .unwrap_or(DEFAULT_CONSOLE_CAPTURE_TIMEOUT_MS);
    if profile.browser == "camoufox" {
      let page = self.get_camoufox_active_page(&profile).await?;
      let target_title = page
        .title()
        .await
        .map_err(|error| Self::camoufox_error("tab title lookup", error))?;
      let target_url = page
        .url()
        .map_err(|error| Self::camoufox_error("tab URL lookup", error))?;
      self.ensure_camoufox_console_capture(&page, true).await?;

      let mut inner = self.inner.lock().await;
      if let Some(existing) = inner.console_captures.get_mut(&args.profile_id) {
        if existing.shared.running.load(Ordering::SeqCst) {
          return Ok(Self::text_tool_result(format!(
            "Console capture is already active for profile '{}'",
            profile.name
          )));
        }

        if let Some(tx) = existing.shutdown_tx.take() {
          let _ = tx.send(());
        }
      }

      let shared = ConsoleCaptureShared {
        logs: Arc::new(AsyncMutex::new(VecDeque::new())),
        running: Arc::new(AtomicBool::new(true)),
        last_error: Arc::new(AsyncMutex::new(None)),
      };
      let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel::<()>();

      inner.console_captures.insert(
        args.profile_id.clone(),
        ConsoleCaptureState {
          shared: shared.clone(),
          shutdown_tx: Some(shutdown_tx),
        },
      );
      drop(inner);

      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "status": "started",
        "timeoutMs": timeout_ms,
        "bufferSize": MAX_CONSOLE_LOG_ENTRIES,
        "target": {
          "id": "camoufox-active-tab",
          "title": target_title,
          "url": target_url,
          "type": "page",
        },
      }));
    }
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let target = self.get_cdp_active_page_target(cdp_port).await?;
    let ws_url = target
      .web_socket_debugger_url
      .clone()
      .ok_or_else(|| McpError::internal("Selected tab is missing webSocketDebuggerUrl"))?;

    let mut inner = self.inner.lock().await;
    if let Some(existing) = inner.console_captures.get_mut(&args.profile_id) {
      if existing.shared.running.load(Ordering::SeqCst) {
        return Ok(Self::text_tool_result(format!(
          "Console capture is already active for profile '{}'",
          profile.name
        )));
      }

      if let Some(tx) = existing.shutdown_tx.take() {
        let _ = tx.send(());
      }
    }

    let shared = ConsoleCaptureShared {
      logs: Arc::new(AsyncMutex::new(VecDeque::new())),
      running: Arc::new(AtomicBool::new(true)),
      last_error: Arc::new(AsyncMutex::new(None)),
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

    inner.console_captures.insert(
      args.profile_id.clone(),
      ConsoleCaptureState {
        shared: shared.clone(),
        shutdown_tx: Some(shutdown_tx),
      },
    );
    drop(inner);

    tokio::spawn(Self::run_console_capture(
      args.profile_id.clone(),
      ws_url,
      shared,
      shutdown_rx,
      ready_tx,
      timeout_ms,
    ));

    match tokio::time::timeout(
      std::time::Duration::from_millis(timeout_ms.max(1_000)),
      ready_rx,
    )
    .await
    {
      Ok(Ok(Ok(()))) => {}
      Ok(Ok(Err(message))) => {
        let mut inner = self.inner.lock().await;
        if let Some(mut state) = inner.console_captures.remove(&args.profile_id) {
          if let Some(tx) = state.shutdown_tx.take() {
            let _ = tx.send(());
          }
        }
        return Err(McpError::internal(message));
      }
      Ok(Err(_)) => {
        let mut inner = self.inner.lock().await;
        inner.console_captures.remove(&args.profile_id);
        return Err(McpError::internal(format!(
          "Console capture task exited before initialization for profile {}",
          args.profile_id
        )));
      }
      Err(_) => {
        let mut inner = self.inner.lock().await;
        if let Some(mut state) = inner.console_captures.remove(&args.profile_id) {
          if let Some(tx) = state.shutdown_tx.take() {
            let _ = tx.send(());
          }
        }
        return Err(McpError::timeout(format!(
          "Timed out waiting for console capture initialization for profile {} after {}ms",
          args.profile_id, timeout_ms
        )));
      }
    }

    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "status": "started",
      "timeoutMs": timeout_ms,
      "bufferSize": MAX_CONSOLE_LOG_ENTRIES,
      "target": {
        "id": target.id,
        "title": target.title,
        "url": target.url,
        "type": target.target_type,
      },
    }))
  }

  async fn handle_get_console_logs(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: GetConsoleLogsArgs = Self::parse_arguments(arguments)?;
    let buffered_capture = {
      let inner = self.inner.lock().await;
      inner
        .console_captures
        .get(&args.profile_id)
        .map(|state| state.shared.clone())
    };
    if let Some(capture) = buffered_capture {
      let logs = capture.logs.lock().await;
      let total = logs.len();
      let limit = args.limit.unwrap_or(DEFAULT_CONSOLE_LOG_LIMIT).max(1);
      let entries: Vec<ConsoleLogEntry> = logs.iter().rev().take(limit).cloned().collect();
      drop(logs);
      let mut entries = entries;
      entries.reverse();

      let last_error = capture.last_error.lock().await.clone();

      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "running": capture.running.load(Ordering::SeqCst),
        "total": total,
        "returned": entries.len(),
        "lastError": last_error,
        "logs": entries,
      }));
    }

    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      let page = self.get_camoufox_active_page(&profile).await?;
      let limit = args.limit.unwrap_or(DEFAULT_CONSOLE_LOG_LIMIT).max(1);
      let value = self
        .evaluate_camoufox_page_value(
          &page,
          &format!(
            r#"(() => {{
              const raw = sessionStorage.getItem("__jnm_mcp_console_capture");
              const parsed = raw ? JSON.parse(raw) : {{ logs: [] }};
              const logs = Array.isArray(parsed.logs) ? parsed.logs : [];
              return {{
                total: logs.length,
                logs: logs.slice(Math.max(0, logs.length - {})),
              }};
            }})()"#,
            limit
          ),
        )
        .await?;
      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "running": true,
        "total": value.get("total").cloned().unwrap_or(serde_json::json!(0)),
        "returned": value.get("logs").and_then(|logs| logs.as_array()).map(|logs| logs.len()).unwrap_or(0),
        "lastError": serde_json::Value::Null,
        "logs": value.get("logs").cloned().unwrap_or_else(|| serde_json::json!([])),
      }));
    }
    Err(McpError::not_found(format!(
      "Console capture not found for profile {}",
      args.profile_id
    )))
  }

  async fn handle_clear_console_logs(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: ClearConsoleLogsArgs = Self::parse_arguments(arguments)?;
    let buffered_capture = {
      let inner = self.inner.lock().await;
      inner
        .console_captures
        .get(&args.profile_id)
        .map(|state| state.shared.clone())
    };
    if let Some(capture) = buffered_capture {
      let mut logs = capture.logs.lock().await;
      let cleared = logs.len();
      logs.clear();
      drop(logs);

      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "cleared": cleared,
        "running": capture.running.load(Ordering::SeqCst),
      }));
    }

    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      let page = self.get_camoufox_active_page(&profile).await?;
      let value = self
        .evaluate_camoufox_page_value(
          &page,
          r#"(() => {
            const raw = sessionStorage.getItem("__jnm_mcp_console_capture");
            const parsed = raw ? JSON.parse(raw) : { logs: [] };
            const logs = Array.isArray(parsed.logs) ? parsed.logs : [];
            const cleared = logs.length;
            sessionStorage.removeItem("__jnm_mcp_console_capture");
            return { cleared };
          })()"#,
        )
        .await?;
      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "cleared": value.get("cleared").cloned().unwrap_or(serde_json::json!(0)),
        "running": true,
      }));
    }
    Err(McpError::not_found(format!(
      "Console capture not found for profile {}",
      args.profile_id
    )))
  }

  async fn handle_start_network_capture(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: StartNetworkCaptureArgs = Self::parse_arguments(arguments)?;
    let profile = self.get_running_profile(&args.profile_id)?;
    let timeout_ms = args
      .options
      .timeout_ms
      .unwrap_or(DEFAULT_NETWORK_CAPTURE_TIMEOUT_MS);
    if profile.browser == "camoufox" {
      let page = self.get_camoufox_active_page(&profile).await?;
      let target_title = page
        .title()
        .await
        .map_err(|error| Self::camoufox_error("tab title lookup", error))?;
      let target_url = page
        .url()
        .map_err(|error| Self::camoufox_error("tab URL lookup", error))?;
      self.ensure_camoufox_network_capture(&page, true).await?;

      let mut inner = self.inner.lock().await;
      if let Some(existing) = inner.network_captures.get_mut(&args.profile_id) {
        if existing.shared.running.load(Ordering::SeqCst) {
          return Ok(Self::text_tool_result(format!(
            "Network capture is already active for profile '{}'",
            profile.name
          )));
        }

        if let Some(tx) = existing.shutdown_tx.take() {
          let _ = tx.send(());
        }
      }

      let shared = NetworkCaptureShared {
        requests: Arc::new(AsyncMutex::new(VecDeque::new())),
        running: Arc::new(AtomicBool::new(true)),
        last_error: Arc::new(AsyncMutex::new(None)),
      };
      let (shutdown_tx, _shutdown_rx) = tokio::sync::oneshot::channel::<()>();
      let (body_request_tx, body_request_rx) = mpsc::channel(1);
      drop(body_request_rx);

      inner.network_captures.insert(
        args.profile_id.clone(),
        NetworkCaptureState {
          shared: shared.clone(),
          shutdown_tx: Some(shutdown_tx),
          body_request_tx,
        },
      );
      drop(inner);

      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "status": "started",
        "timeoutMs": timeout_ms,
        "bufferSize": MAX_NETWORK_REQUEST_ENTRIES,
        "target": {
          "id": "camoufox-active-tab",
          "title": target_title,
          "url": target_url,
          "type": "page",
        },
      }));
    }
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let target = self.get_cdp_active_page_target(cdp_port).await?;
    let ws_url = target
      .web_socket_debugger_url
      .clone()
      .ok_or_else(|| McpError::internal("Selected tab is missing webSocketDebuggerUrl"))?;

    let mut inner = self.inner.lock().await;
    if let Some(existing) = inner.network_captures.get_mut(&args.profile_id) {
      if existing.shared.running.load(Ordering::SeqCst) {
        return Ok(Self::text_tool_result(format!(
          "Network capture is already active for profile '{}'",
          profile.name
        )));
      }

      if let Some(tx) = existing.shutdown_tx.take() {
        let _ = tx.send(());
      }
    }

    let shared = NetworkCaptureShared {
      requests: Arc::new(AsyncMutex::new(VecDeque::new())),
      running: Arc::new(AtomicBool::new(true)),
      last_error: Arc::new(AsyncMutex::new(None)),
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let (body_request_tx, body_request_rx) = mpsc::channel(32);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();

    inner.network_captures.insert(
      args.profile_id.clone(),
      NetworkCaptureState {
        shared: shared.clone(),
        shutdown_tx: Some(shutdown_tx),
        body_request_tx,
      },
    );
    drop(inner);

    tokio::spawn(Self::run_network_capture(
      args.profile_id.clone(),
      ws_url,
      shared,
      shutdown_rx,
      body_request_rx,
      ready_tx,
      timeout_ms,
    ));

    match tokio::time::timeout(
      std::time::Duration::from_millis(timeout_ms.max(1_000)),
      ready_rx,
    )
    .await
    {
      Ok(Ok(Ok(()))) => {}
      Ok(Ok(Err(message))) => {
        let mut inner = self.inner.lock().await;
        if let Some(mut state) = inner.network_captures.remove(&args.profile_id) {
          if let Some(tx) = state.shutdown_tx.take() {
            let _ = tx.send(());
          }
        }
        return Err(McpError::internal(message));
      }
      Ok(Err(_)) => {
        let mut inner = self.inner.lock().await;
        inner.network_captures.remove(&args.profile_id);
        return Err(McpError::internal(format!(
          "Network capture task exited before initialization for profile {}",
          args.profile_id
        )));
      }
      Err(_) => {
        let mut inner = self.inner.lock().await;
        if let Some(mut state) = inner.network_captures.remove(&args.profile_id) {
          if let Some(tx) = state.shutdown_tx.take() {
            let _ = tx.send(());
          }
        }
        return Err(McpError::timeout(format!(
          "Timed out waiting for network capture initialization for profile {} after {}ms",
          args.profile_id, timeout_ms
        )));
      }
    }

    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "status": "started",
      "timeoutMs": timeout_ms,
      "bufferSize": MAX_NETWORK_REQUEST_ENTRIES,
      "target": {
        "id": target.id,
        "title": target.title,
        "url": target.url,
        "type": target.target_type,
      },
    }))
  }

  async fn handle_get_network_requests(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: GetNetworkRequestsArgs = Self::parse_arguments(arguments)?;
    let buffered_capture = {
      let inner = self.inner.lock().await;
      inner
        .network_captures
        .get(&args.profile_id)
        .map(|state| state.shared.clone())
    };
    if let Some(capture) = buffered_capture {
      let requests = capture.requests.lock().await;
      let total = requests.len();
      let limit = args.limit.unwrap_or(DEFAULT_NETWORK_REQUEST_LIMIT).max(1);
      let entries: Vec<NetworkRequestEntry> = requests.iter().rev().take(limit).cloned().collect();
      drop(requests);
      let mut entries = entries;
      entries.reverse();

      let last_error = capture.last_error.lock().await.clone();

      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "running": capture.running.load(Ordering::SeqCst),
        "total": total,
        "returned": entries.len(),
        "lastError": last_error,
        "requests": entries,
      }));
    }

    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      let page = self.get_camoufox_active_page(&profile).await?;
      let limit = args.limit.unwrap_or(DEFAULT_NETWORK_REQUEST_LIMIT).max(1);
      let value = self
        .evaluate_camoufox_page_value(
          &page,
          &format!(
            r#"(() => {{
              const raw = sessionStorage.getItem("__jnm_mcp_network_capture");
              const parsed = raw ? JSON.parse(raw) : {{ requests: [] }};
              const requests = Array.isArray(parsed.requests) ? parsed.requests : [];
              return {{
                total: requests.length,
                requests: requests.slice(Math.max(0, requests.length - {})),
              }};
            }})()"#,
            limit
          ),
        )
        .await?;
      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "running": true,
        "total": value.get("total").cloned().unwrap_or(serde_json::json!(0)),
        "returned": value.get("requests").and_then(|items| items.as_array()).map(|items| items.len()).unwrap_or(0),
        "lastError": serde_json::Value::Null,
        "requests": value.get("requests").cloned().unwrap_or_else(|| serde_json::json!([])),
      }));
    }
    Err(McpError::not_found(format!(
      "Network capture not found for profile {}",
      args.profile_id
    )))
  }

  async fn handle_get_response_body(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: GetResponseBodyArgs = Self::parse_arguments(arguments)?;
    let capture_state = {
      let inner = self.inner.lock().await;
      inner
        .network_captures
        .get(&args.profile_id)
        .map(|state| (state.shared.clone(), state.body_request_tx.clone()))
    };

    let profile = self.get_running_profile(&args.profile_id).ok();
    if profile
      .as_ref()
      .map(|profile| profile.browser == "camoufox")
      .unwrap_or(false)
      && capture_state.is_none()
    {
      let profile = profile.expect("camoufox profile existence checked above");
      let page = self.get_camoufox_active_page(&profile).await?;
      let max_bytes = args.max_bytes.unwrap_or(DEFAULT_RESPONSE_BODY_MAX_BYTES);
      let value = self
        .evaluate_camoufox_page_value(
          &page,
          &format!(
            r#"(() => {{
              const raw = sessionStorage.getItem("__jnm_mcp_network_capture");
              const parsed = raw ? JSON.parse(raw) : {{ requests: [] }};
              const requests = Array.isArray(parsed.requests) ? parsed.requests : [];
              const entry = requests.find((item) => item.requestId === {});
              if (!entry) {{
                return {{ found: false }};
              }}
              return {{
                found: true,
                failed: !!entry.failed,
                body: entry.responseBody,
                base64Encoded: !!entry.responseBodyBase64Encoded,
              }};
            }})()"#,
            serde_json::to_string(&args.request_id)
              .map_err(|e| McpError::internal(format!("Failed to serialize request id: {e}")))?,
          ),
        )
        .await?;

      if value.get("found").and_then(|value| value.as_bool()) != Some(true) {
        return Err(McpError::not_found(format!(
          "Request {} was not found in captured network entries for profile {}",
          args.request_id, args.profile_id
        )));
      }
      if value.get("failed").and_then(|value| value.as_bool()) == Some(true) {
        return Err(McpError::not_found(format!(
          "Response body is unavailable for request {} because the request failed",
          args.request_id
        )));
      }
      let body = value
        .get("body")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
      let base64_encoded = value
        .get("base64Encoded")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
      let byte_len = if base64_encoded {
        base64::engine::general_purpose::STANDARD
          .decode(body.as_bytes())
          .map_err(|e| McpError::internal(format!("Failed to decode base64 response body: {e}")))?
          .len()
      } else {
        body.len()
      };
      if byte_len > max_bytes {
        return Err(McpError::invalid_params(format!(
          "Response body for request {} is {} bytes, which exceeds max_bytes {}",
          args.request_id, byte_len, max_bytes
        )));
      }
      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "requestId": args.request_id,
        "base64Encoded": base64_encoded,
        "size": byte_len,
        "body": body,
      }));
    }
    let max_bytes = args.max_bytes.unwrap_or(DEFAULT_RESPONSE_BODY_MAX_BYTES);
    let (capture, body_request_tx) = capture_state.ok_or_else(|| {
      if profile.is_none() {
        McpError::not_found(format!("Profile not found: {}", args.profile_id))
      } else {
        McpError::not_found(format!(
          "Network capture not found for profile {}",
          args.profile_id
        ))
      }
    })?;

    let mut cached_body = None;
    let mut cached_base64_encoded = false;
    let wait_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
      let mut found = false;
      {
        let requests = capture.requests.lock().await;
        if let Some(entry) = requests
          .iter()
          .find(|entry| entry.request_id == args.request_id)
        {
          found = true;
          if let Some(body) = entry.response_body.clone() {
            cached_base64_encoded = entry.response_body_base64_encoded;
            cached_body = Some(body);
          } else if entry.failed {
            return Err(McpError::not_found(format!(
              "Response body is unavailable for request {} because the request failed",
              args.request_id
            )));
          } else if !entry.has_response_body
            && capture.running.load(Ordering::SeqCst)
            && std::time::Instant::now() < wait_deadline
          {
            // Keep waiting for loadingFinished / body cache to land.
          } else {
            break;
          }
        }
      }

      if cached_body.is_some() {
        break;
      }
      if !found {
        return Err(McpError::not_found(format!(
          "Request {} was not found in captured network entries for profile {}",
          args.request_id, args.profile_id
        )));
      }
      if !capture.running.load(Ordering::SeqCst) || std::time::Instant::now() >= wait_deadline {
        break;
      }
      tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let (body, base64_encoded) = if let Some(body) = cached_body {
      (body, cached_base64_encoded)
    } else {
      let (response_tx, response_rx) = tokio::sync::oneshot::channel();
      body_request_tx
        .send(BodyFetchRequest {
          request_id: args.request_id.clone(),
          response_tx,
        })
        .await
        .map_err(|_| {
          McpError::internal(format!(
            "Network capture worker is unavailable for profile {}",
            args.profile_id
          ))
        })?;

      match tokio::time::timeout(std::time::Duration::from_secs(10), response_rx).await {
        Ok(Ok(Ok(result))) => result,
        Ok(Ok(Err(message))) => return Err(McpError::not_found(message)),
        Ok(Err(_)) => {
          return Err(McpError::internal(format!(
            "Response body request channel closed for profile {}",
            args.profile_id
          )));
        }
        Err(_) => {
          return Err(McpError::timeout(format!(
            "Timed out waiting for response body for request {}",
            args.request_id
          )));
        }
      }
    };

    let byte_len = if base64_encoded {
      base64::engine::general_purpose::STANDARD
        .decode(body.as_bytes())
        .map_err(|e| McpError::internal(format!("Failed to decode base64 response body: {e}")))?
        .len()
    } else {
      body.len()
    };

    if byte_len > max_bytes {
      return Err(McpError::invalid_params(format!(
        "Response body for request {} is {} bytes, which exceeds max_bytes {}",
        args.request_id, byte_len, max_bytes
      )));
    }

    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "requestId": args.request_id,
      "base64Encoded": base64_encoded,
      "byteLength": byte_len,
      "body": body,
    }))
  }

  async fn handle_clear_network_requests(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: ClearNetworkRequestsArgs = Self::parse_arguments(arguments)?;
    let buffered_capture = {
      let inner = self.inner.lock().await;
      inner
        .network_captures
        .get(&args.profile_id)
        .map(|state| state.shared.clone())
    };
    if let Some(capture) = buffered_capture {
      let mut requests = capture.requests.lock().await;
      let cleared = requests.len();
      requests.clear();
      drop(requests);

      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "cleared": cleared,
        "running": capture.running.load(Ordering::SeqCst),
      }));
    }

    let profile = self.get_running_profile(&args.profile_id)?;
    if profile.browser == "camoufox" {
      let page = self.get_camoufox_active_page(&profile).await?;
      let value = self
        .evaluate_camoufox_page_value(
          &page,
          r#"(() => {
            const raw = sessionStorage.getItem("__jnm_mcp_network_capture");
            const parsed = raw ? JSON.parse(raw) : { requests: [] };
            const requests = Array.isArray(parsed.requests) ? parsed.requests : [];
            const cleared = requests.length;
            sessionStorage.removeItem("__jnm_mcp_network_capture");
            return { cleared };
          })()"#,
        )
        .await?;
      return Self::json_tool_result(&serde_json::json!({
        "profileId": args.profile_id,
        "cleared": value.get("cleared").cloned().unwrap_or(serde_json::json!(0)),
        "running": true,
      }));
    }
    Err(McpError::not_found(format!(
      "Network capture not found for profile {}",
      args.profile_id
    )))
  }

  async fn handle_get_all_traffic_snapshots(&self) -> Result<serde_json::Value, McpError> {
    let snapshots = crate::traffic_stats::get_all_traffic_snapshots_realtime();
    let omitted_unscoped = snapshots
      .iter()
      .filter(|snapshot| snapshot.profile_id.is_none())
      .count();
    let profile_snapshots: Vec<_> = snapshots
      .into_iter()
      .filter(|snapshot| snapshot.profile_id.is_some())
      .collect();

    Self::json_tool_result(&serde_json::json!({
      "snapshots": profile_snapshots,
      "omittedUnscoped": omitted_unscoped,
      "note": "Unscoped proxy/global traffic entries are omitted from MCP profile snapshots to avoid ambiguous profile_id=null rows."
    }))
  }

  async fn handle_get_traffic_stats_for_period(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: GetTrafficStatsForPeriodArgs = Self::parse_arguments(arguments)?;
    let stats = crate::traffic_stats::get_traffic_stats_for_period(&args.profile_id, args.seconds);
    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "seconds": args.seconds,
      "stats": stats,
    }))
  }

  async fn handle_clear_all_traffic_stats(&self) -> Result<serde_json::Value, McpError> {
    crate::traffic_stats::clear_all_traffic_stats()
      .map_err(|e| McpError::internal(format!("Failed to clear traffic stats: {e}")))?;
    Ok(Self::text_tool_result("Cleared all traffic statistics"))
  }

  async fn handle_read_profile_cookies(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: ReadProfileCookiesArgs = Self::parse_arguments(arguments)?;
    let result = crate::cookie_manager::CookieManager::read_cookies(&args.profile_id)
      .map_err(McpError::internal)?;
    Self::json_tool_result(&result)
  }

  async fn handle_copy_profile_cookies(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: CopyProfileCookiesArgs = Self::parse_arguments(arguments)?;
    let app_handle = self.get_app_handle().await?;
    let request = crate::cookie_manager::CookieCopyRequest {
      source_profile_id: args.source_profile_id,
      target_profile_ids: args.target_profile_ids.clone(),
      selected_cookies: args
        .selected_cookies
        .into_iter()
        .map(|cookie| crate::cookie_manager::SelectedCookie {
          domain: cookie.domain,
          name: cookie.name.unwrap_or_default(),
        })
        .collect(),
    };

    let results = crate::cookie_manager::CookieManager::copy_cookies(&app_handle, request)
      .await
      .map_err(McpError::internal)?;
    Self::queue_sync_for_profiles_if_enabled(&args.target_profile_ids).await;
    Self::json_tool_result(&results)
  }

  async fn handle_import_cookies_from_file(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: ImportCookiesFromFileArgs = Self::parse_arguments(arguments)?;
    let app_handle = self.get_app_handle().await?;
    let result = crate::cookie_manager::CookieManager::import_cookies(
      &app_handle,
      &args.profile_id,
      &args.content,
    )
    .await
    .map_err(McpError::internal)?;
    Self::queue_sync_for_profiles_if_enabled(std::slice::from_ref(&args.profile_id)).await;
    Self::json_tool_result(&result)
  }

  async fn handle_export_profile_cookies(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: ExportProfileCookiesArgs = Self::parse_arguments(arguments)?;
    let content =
      crate::cookie_manager::CookieManager::export_cookies(&args.profile_id, &args.format)
        .map_err(McpError::internal)?;
    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "format": args.format,
      "content": content,
    }))
  }

  async fn handle_get_local_storage(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: GetStorageArgs = Self::parse_arguments(arguments)?;
    let value = self
      .handle_get_web_storage(
        &args.profile_id,
        "localStorage",
        "local",
        args.key.as_deref(),
      )
      .await?;
    Self::json_tool_result(&value)
  }

  async fn handle_set_local_storage(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: SetStorageArgs = Self::parse_arguments(arguments)?;
    let (stored_value, serialized_as) = Self::prepare_storage_value(&args.value)?;
    let key_literal = serde_json::to_string(&args.key)
      .map_err(|e| McpError::internal(format!("Failed to serialize storage key: {e}")))?;
    let value_literal = serde_json::to_string(&stored_value)
      .map_err(|e| McpError::internal(format!("Failed to serialize storage payload: {e}")))?;

    let expression = format!(
      r#"(() => {{
        const storage = window.localStorage;
        const key = {key_literal};
        const value = {value_literal};
        storage.setItem(key, value);
        let parsedValue = null;
        let parsedAsJson = false;
        try {{
          parsedValue = JSON.parse(value);
          parsedAsJson = true;
        }} catch (_error) {{}}
        return {{
          storage: "local",
          key,
          rawValue: value,
          parsedValue,
          parsedAsJson,
          serializedAs: "{serialized_as}",
        }};
      }})()"#
    );

    let value = self
      .evaluate_javascript_value(&args.profile_id, &expression)
      .await?;
    Self::json_tool_result(&value)
  }

  async fn handle_get_session_storage(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: GetStorageArgs = Self::parse_arguments(arguments)?;
    let value = self
      .handle_get_web_storage(
        &args.profile_id,
        "sessionStorage",
        "session",
        args.key.as_deref(),
      )
      .await?;
    Self::json_tool_result(&value)
  }

  async fn handle_clear_storage(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: ClearStorageArgs = Self::parse_arguments(arguments)?;
    let storage = args.storage.as_deref().unwrap_or("both");
    if !matches!(storage, "local" | "session" | "both") {
      return Err(McpError::invalid_params(format!(
        "Unsupported storage scope: {storage}"
      )));
    }

    let key_literal = serde_json::to_string(&args.key)
      .map_err(|e| McpError::internal(format!("Failed to serialize storage key: {e}")))?;
    let expression = format!(
      r#"(() => {{
        const key = {key_literal};
        const targets = [];
        if ("{storage}" === "local" || "{storage}" === "both") {{
          targets.push({{ name: "local", storage: window.localStorage }});
        }}
        if ("{storage}" === "session" || "{storage}" === "both") {{
          targets.push({{ name: "session", storage: window.sessionStorage }});
        }}

        const results = targets.map((target) => {{
          if (key !== null) {{
            const existed = target.storage.getItem(key) !== null;
            target.storage.removeItem(key);
            return {{
              storage: target.name,
              key,
              existed,
              count: target.storage.length,
            }};
          }}

          const countBefore = target.storage.length;
          target.storage.clear();
          return {{
            storage: target.name,
            key: null,
            existed: countBefore > 0,
            clearedEntries: countBefore,
            count: target.storage.length,
          }};
        }});

        return {{
          scope: "{storage}",
          key,
          results,
        }};
      }})()"#
    );

    let value = self
      .evaluate_javascript_value(&args.profile_id, &expression)
      .await?;
    Self::json_tool_result(&value)
  }

  async fn handle_get_web_storage(
    &self,
    profile_id: &str,
    storage_accessor: &str,
    storage_name: &str,
    key: Option<&str>,
  ) -> Result<serde_json::Value, McpError> {
    let key_literal = serde_json::to_string(&key)
      .map_err(|e| McpError::internal(format!("Failed to serialize storage key: {e}")))?;
    let expression = format!(
      r#"(() => {{
        const storage = window.{storage_accessor};
        const key = {key_literal};

        if (key !== null) {{
          const rawValue = storage.getItem(key);
          let parsedValue = null;
          let parsedAsJson = false;
          if (rawValue !== null) {{
            try {{
              parsedValue = JSON.parse(rawValue);
              parsedAsJson = true;
            }} catch (_error) {{}}
          }}
          return {{
            storage: "{storage_name}",
            key,
            exists: rawValue !== null,
            rawValue,
            parsedValue,
            parsedAsJson,
          }};
        }}

        const entries = {{}};
        const parsedEntries = {{}};
        const jsonKeys = [];

        for (let index = 0; index < storage.length; index += 1) {{
          const entryKey = storage.key(index);
          const rawValue = storage.getItem(entryKey);
          entries[entryKey] = rawValue;
          try {{
            parsedEntries[entryKey] = JSON.parse(rawValue);
            jsonKeys.push(entryKey);
          }} catch (_error) {{}}
        }}

        return {{
          storage: "{storage_name}",
          count: storage.length,
          entries,
          parsedEntries,
          jsonKeys,
        }};
      }})()"#
    );

    self
      .evaluate_javascript_value(profile_id, &expression)
      .await
  }

  async fn handle_wait_for_selector(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: WaitForSelectorArgs = Self::parse_arguments(arguments)?;
    args.locator.validate()?;
    if let Some(frame) = args.frame.as_ref() {
      frame.validate()?;
    }
    let state = args.state.as_deref().unwrap_or("visible");
    if !matches!(state, "exists" | "visible") {
      return Err(McpError::invalid_params(format!(
        "Unsupported wait state: {state}"
      )));
    }

    let profile = self.get_running_profile(&args.profile_id)?;
    let cdp_port = self.get_cdp_port_for_profile(&profile).await?;
    let ws_url = self.get_cdp_ws_url(cdp_port).await?;
    let (frame_selector, resolved_frame, context_id) = self
      .resolve_frame_context(&args.profile_id, &ws_url, args.frame.as_ref())
      .await?;
    let timeout_ms = args.options.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
    let expression = Self::build_locator_condition_expression(&args.locator, state)?;
    let wait_locator = args.locator.clone();
    let last_observed = Arc::new(Mutex::new(None::<serde_json::Value>));

    let result = self
      .wait_with_timeout(
        timeout_ms,
        || {
          let last_observed = last_observed.clone();
          let ws_url = ws_url.clone();
          let expression = expression.clone();
          let wait_locator = wait_locator.clone();
          async move {
            let value = self
              .evaluate_runtime_value(&ws_url, &expression, context_id)
              .await?;

            *last_observed.lock().expect("selector wait state lock poisoned") = Some(value.clone());
            if let Some(error) = value.get("error").and_then(|value| value.as_str()) {
              return Err(McpError::invalid_params(error));
            }

            let snapshot = Self::parse_locator_resolution_snapshot(value.clone())?;
            Self::ensure_single_target_locator(&wait_locator, &snapshot, "wait_for_selector")?;

            if value.get("ok").and_then(|value| value.as_bool()) == Some(true) {
              Ok(Some(value))
            } else {
              Ok(None)
            }
          }
        },
        || {
          let last_observed = last_observed
            .lock()
            .expect("selector wait state lock poisoned")
            .clone()
            .unwrap_or(serde_json::Value::Null);
          format!(
            "Timed out after {timeout_ms}ms waiting for locator {:?} to become {state}. Last observed state: {}",
            args.locator,
            serde_json::to_string(&last_observed).unwrap_or_else(|_| "null".to_string())
          )
        },
      )
      .await?;

    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "frame": frame_selector,
      "resolvedFrame": resolved_frame,
      "state": state,
      "timeoutMs": timeout_ms,
      "locator": args.locator,
      "result": result,
    }))
  }

  async fn handle_wait_for_text(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: WaitForTextArgs = Self::parse_arguments(arguments)?;
    let exact = args.exact.unwrap_or(false);
    let timeout_ms = args.options.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
    let expression = Self::build_wait_for_text_expression(
      &args.text,
      args.selector.as_deref(),
      args.locator.as_ref(),
      exact,
    )?;
    let last_observed = Arc::new(Mutex::new(None::<serde_json::Value>));
    let profile_id = args.profile_id.clone();

    let result = self
      .wait_with_timeout(
        timeout_ms,
        || {
          let last_observed = last_observed.clone();
          let profile_id = profile_id.clone();
          let expression = expression.clone();
          async move {
            let value = self
              .evaluate_javascript_value(&profile_id, &expression)
              .await?;

            *last_observed.lock().expect("text wait state lock poisoned") = Some(value.clone());
            if let Some(error) = value.get("error").and_then(|value| value.as_str()) {
              return Err(McpError::invalid_params(error));
            }

            if value.get("ok").and_then(|value| value.as_bool()) == Some(true) {
              Ok(Some(value))
            } else {
              Ok(None)
            }
          }
        },
        || {
          let last_observed = last_observed
            .lock()
            .expect("text wait state lock poisoned")
            .clone()
            .unwrap_or(serde_json::Value::Null);
          format!(
            "Timed out after {timeout_ms}ms waiting for text {:?}. Last observed state: {}",
            args.text,
            serde_json::to_string(&last_observed).unwrap_or_else(|_| "null".to_string())
          )
        },
      )
      .await?;

    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "text": args.text,
      "exact": exact,
      "timeoutMs": timeout_ms,
      "scope": match (args.selector, args.locator) {
        (Some(selector), None) => serde_json::json!({
          "type": "selector",
          "selector": selector,
        }),
        (None, Some(locator)) => serde_json::json!({
          "type": "locator",
          "locator": locator,
        }),
        (None, None) => serde_json::json!({
          "type": "page",
        }),
        (Some(selector), Some(locator)) => serde_json::json!({
          "type": "invalid",
          "selector": selector,
          "locator": locator,
        }),
      },
      "result": result,
    }))
  }

  async fn handle_wait_for_navigation(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: WaitForNavigationArgs = Self::parse_arguments(arguments)?;
    if matches!(args.from_url.as_deref(), Some(url) if url.trim().is_empty()) {
      return Err(McpError::invalid_params("from_url cannot be empty"));
    }

    let timeout_ms = args.options.timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
    let expression = Self::build_navigation_snapshot_expression();
    let initial_snapshot = Self::parse_navigation_snapshot(
      self
        .evaluate_javascript_value(&args.profile_id, expression)
        .await?,
    )?;
    let baseline_url = args
      .from_url
      .clone()
      .unwrap_or_else(|| initial_snapshot.url.clone());
    let saw_non_complete = Arc::new(AtomicBool::new(initial_snapshot.ready_state != "complete"));
    let last_observed = Arc::new(Mutex::new(Some(serde_json::json!({
      "url": initial_snapshot.url.clone(),
      "title": initial_snapshot.title.clone(),
      "readyState": initial_snapshot.ready_state.clone(),
      "ok": false,
    }))));
    let profile_id = args.profile_id.clone();

    let result = self
      .wait_with_timeout(
        timeout_ms,
        || {
          let last_observed = last_observed.clone();
          let saw_non_complete = saw_non_complete.clone();
          let profile_id = profile_id.clone();
          let baseline_url = baseline_url.clone();
          async move {
            let current = Self::parse_navigation_snapshot(
              self
                .evaluate_javascript_value(&profile_id, expression)
                .await?,
            )?;

            if current.ready_state != "complete" {
              saw_non_complete.store(true, Ordering::SeqCst);
            }

            let ok = Self::navigation_wait_satisfied(
              &baseline_url,
              &current,
              saw_non_complete.load(Ordering::SeqCst),
            );
            let payload = serde_json::json!({
              "ok": ok,
              "url": current.url,
              "title": current.title,
              "readyState": current.ready_state,
              "baselineUrl": baseline_url,
              "sawNonComplete": saw_non_complete.load(Ordering::SeqCst),
            });
            *last_observed.lock().expect("navigation wait state lock poisoned") =
              Some(payload.clone());

            if ok {
              Ok(Some(payload))
            } else {
              Ok(None)
            }
          }
        },
        || {
          let last_observed = last_observed
            .lock()
            .expect("navigation wait state lock poisoned")
            .clone()
            .unwrap_or(serde_json::Value::Null);
          format!(
            "Timed out after {timeout_ms}ms waiting for navigation from {:?}. Last observed state: {}",
            args.from_url,
            serde_json::to_string(&last_observed).unwrap_or_else(|_| "null".to_string())
          )
        },
      )
      .await?;

    Self::json_tool_result(&serde_json::json!({
      "profileId": args.profile_id,
      "timeoutMs": timeout_ms,
      "fromUrl": args.from_url,
      "initial": initial_snapshot,
      "result": result,
    }))
  }

  async fn handle_generate_sample_fingerprint(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let args: GenerateSampleFingerprintArgs = Self::parse_arguments(arguments)?;
    let app_handle = self.get_app_handle().await?;
    let temp_profile = crate::profile::BrowserProfile {
      id: uuid::Uuid::new_v4(),
      name: "temp_fingerprint_gen".to_string(),
      browser: args.browser.clone(),
      version: args.version.clone(),
      process_id: None,
      proxy_id: None,
      vpn_id: None,
      launch_hook: None,
      last_launch: None,
      release_type: "stable".to_string(),
      camoufox_config: None,
      chromium_config: None,
      group_id: None,
      tags: Vec::new(),
      note: None,
      sync_mode: crate::profile::types::SyncMode::Disabled,
      encryption_salt: None,
      last_sync: None,
      host_os: None,
      ephemeral: false,
      extension_group_id: None,
      proxy_bypass_rules: Vec::new(),
      created_by_id: None,
      created_by_email: None,
      dns_blocklist: None,
    };

    let config_json = serde_json::to_string(&args.config)
      .map_err(|e| McpError::internal(format!("Failed to serialize fingerprint config: {e}")))?;

    let fingerprint = if args.browser == "camoufox" {
      let config: crate::camoufox_manager::CamoufoxConfig = serde_json::from_str(&config_json)
        .map_err(|e| McpError::invalid_params(format!("Failed to parse Camoufox config: {e}")))?;
      crate::camoufox_manager::CamoufoxManager::instance()
        .generate_fingerprint_config(&app_handle, &temp_profile, &config)
        .await
        .map_err(|e| McpError::internal(format!("Failed to generate fingerprint: {e}")))?
    } else if is_mcp_chromium_browser(&args.browser) {
      let config: crate::chromium_manager::ChromiumConfig = serde_json::from_str(&config_json)
        .map_err(|e| McpError::invalid_params(format!("Failed to parse Chromium config: {e}")))?;
      crate::chromium_manager::ChromiumManager::instance()
        .generate_fingerprint_config(&app_handle, &temp_profile, &config)
        .await
        .map_err(|e| McpError::internal(format!("Failed to generate fingerprint: {e}")))?
    } else {
      return Err(McpError::invalid_params(format!(
        "Unsupported browser for fingerprint generation: {}",
        args.browser
      )));
    };

    Self::json_tool_result(&serde_json::json!({
      "browser": canonical_mcp_browser_name(&args.browser),
      "version": args.version,
      "fingerprint": fingerprint,
      "nextStep": {
        "tool": "update_profile_fingerprint",
        "argumentsTemplate": {
          "profile_id": "PROFILE_UUID",
          "fingerprint": fingerprint,
        }
      }
    }))
  }

  async fn handle_is_geoip_database_available(&self) -> Result<serde_json::Value, McpError> {
    Self::json_tool_result(&serde_json::json!({
      "available": crate::geoip_downloader::GeoIPDownloader::is_geoip_database_available()
    }))
  }

  async fn handle_download_geoip_database(&self) -> Result<serde_json::Value, McpError> {
    let app_handle = self.get_app_handle().await?;
    crate::geoip_downloader::GeoIPDownloader::instance()
      .download_geoip_database(&app_handle)
      .await
      .map_err(|e| McpError::internal(format!("Failed to download GeoIP database: {e}")))?;
    Ok(Self::text_tool_result(
      "GeoIP database download completed or is already in progress",
    ))
  }

  // --- Synchronizer handlers ---

  async fn handle_start_sync_session(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let leader_id = arguments
      .get("leader_profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing leader_profile_id".to_string(),
      })?;
    let follower_ids: Vec<String> = arguments
      .get("follower_profile_ids")
      .and_then(|v| v.as_array())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing follower_profile_ids".to_string(),
      })?
      .iter()
      .filter_map(|v| v.as_str().map(|s| s.to_string()))
      .collect();

    let app = {
      let inner = self.inner.lock().await;
      inner.app_handle.clone().ok_or_else(|| McpError {
        code: -32000,
        message: "MCP server not properly initialized".to_string(),
      })?
    };

    let info = crate::synchronizer::SynchronizerManager::instance()
      .start_session(app, leader_id.to_string(), follower_ids)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&info).unwrap_or_default()
      }]
    }))
  }

  async fn handle_stop_sync_session(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let session_id = arguments
      .get("session_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing session_id".to_string(),
      })?;

    let app = {
      let inner = self.inner.lock().await;
      inner.app_handle.clone().ok_or_else(|| McpError {
        code: -32000,
        message: "MCP server not properly initialized".to_string(),
      })?
    };

    crate::synchronizer::SynchronizerManager::instance()
      .stop_session(app, session_id)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": "Sync session stopped"
      }]
    }))
  }

  async fn handle_get_sync_sessions(&self) -> Result<serde_json::Value, McpError> {
    let sessions = crate::synchronizer::SynchronizerManager::instance()
      .get_sessions()
      .await;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": serde_json::to_string_pretty(&sessions).unwrap_or_default()
      }]
    }))
  }

  async fn handle_remove_sync_follower(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let session_id = arguments
      .get("session_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing session_id".to_string(),
      })?;
    let follower_id = arguments
      .get("follower_profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing follower_profile_id".to_string(),
      })?;

    let app = {
      let inner = self.inner.lock().await;
      inner.app_handle.clone().ok_or_else(|| McpError {
        code: -32000,
        message: "MCP server not properly initialized".to_string(),
      })?
    };

    crate::synchronizer::SynchronizerManager::instance()
      .remove_follower(app, session_id, follower_id)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;

    Ok(serde_json::json!({
      "content": [{
        "type": "text",
        "text": "Follower removed from sync session"
      }]
    }))
  }

  async fn handle_start_action_recording(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let profile_id = arguments
      .get("profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing profile_id".to_string(),
      })?
      .to_string();
    let app = self.get_app_handle().await?;
    let info = crate::recorder::commands::start_recording(app, profile_id)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Self::json_tool_result(&info)
  }

  async fn handle_stop_action_recording(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let session_id = arguments
      .get("session_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing session_id".to_string(),
      })?
      .to_string();
    let app = self.get_app_handle().await?;
    let recording = crate::recorder::commands::stop_recording(app, session_id)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Self::json_tool_result(&recording)
  }

  async fn handle_get_recorded_events(&self) -> Result<serde_json::Value, McpError> {
    let sessions = crate::recorder::commands::get_recorder_sessions()
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Self::json_tool_result(&sessions)
  }

  async fn handle_list_recordings(&self) -> Result<serde_json::Value, McpError> {
    let recordings = crate::recorder::commands::list_recordings()
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Self::json_tool_result(&recordings)
  }

  async fn handle_export_recording_as_recipe(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let recording_id = arguments
      .get("recording_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing recording_id".to_string(),
      })?
      .to_string();
    let recipe_name = arguments
      .get("recipe_name")
      .and_then(|v| v.as_str())
      .map(ToString::to_string);
    let recipe = crate::recorder::commands::export_recording_as_recipe(recording_id, recipe_name)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Self::json_tool_result(&recipe)
  }

  async fn handle_replay_recording(
    &self,
    arguments: &serde_json::Value,
  ) -> Result<serde_json::Value, McpError> {
    let recording_id = arguments
      .get("recording_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing recording_id".to_string(),
      })?
      .to_string();
    let profile_id = arguments
      .get("profile_id")
      .and_then(|v| v.as_str())
      .ok_or_else(|| McpError {
        code: -32602,
        message: "Missing profile_id".to_string(),
      })?
      .to_string();
    crate::recorder::commands::replay_recording(recording_id, profile_id)
      .await
      .map_err(|e| McpError {
        code: -32000,
        message: e,
      })?;
    Ok(Self::text_tool_result("Recording replay started"))
  }
}

lazy_static::lazy_static! {
  static ref MCP_SERVER: McpServer = McpServer::new();
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::VecDeque;

  fn parse_text_tool_payload(value: serde_json::Value) -> serde_json::Value {
    let text = value
      .get("content")
      .and_then(|content| content.as_array())
      .and_then(|content| content.first())
      .and_then(|item| item.get("text"))
      .and_then(|text| text.as_str())
      .expect("expected text tool result");

    serde_json::from_str(text).expect("expected JSON-encoded text payload")
  }

  #[test]
  fn test_mcp_tools_count() {
    let server = McpServer::new();
    let tools = server.get_tools();

    assert!(tools.len() >= 77);

    // Check tool names
    let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    // Profile tools
    assert!(tool_names.contains(&"list_profiles"));
    assert!(tool_names.contains(&"get_profile"));
    assert!(tool_names.contains(&"run_profile"));
    assert!(tool_names.contains(&"kill_profile"));
    assert!(tool_names.contains(&"get_profile_status"));
    // Group tools
    assert!(tool_names.contains(&"list_groups"));
    assert!(tool_names.contains(&"get_group"));
    assert!(tool_names.contains(&"create_group"));
    assert!(tool_names.contains(&"update_group"));
    assert!(tool_names.contains(&"delete_group"));
    assert!(tool_names.contains(&"assign_profiles_to_group"));
    // Proxy tools
    assert!(tool_names.contains(&"list_proxies"));
    assert!(tool_names.contains(&"get_proxy"));
    assert!(tool_names.contains(&"create_proxy"));
    assert!(tool_names.contains(&"update_proxy"));
    assert!(tool_names.contains(&"delete_proxy"));
    // Proxy import/export tools
    assert!(tool_names.contains(&"export_proxies"));
    assert!(tool_names.contains(&"import_proxies"));
    // VPN tools
    assert!(tool_names.contains(&"import_vpn"));
    assert!(tool_names.contains(&"list_vpn_configs"));
    assert!(tool_names.contains(&"delete_vpn"));
    assert!(tool_names.contains(&"connect_vpn"));
    assert!(tool_names.contains(&"disconnect_vpn"));
    assert!(tool_names.contains(&"get_vpn_status"));
    // Fingerprint tools
    assert!(tool_names.contains(&"get_profile_fingerprint"));
    assert!(tool_names.contains(&"update_profile_fingerprint"));
    assert!(tool_names.contains(&"update_profile_proxy_bypass_rules"));
    // Extension tools
    assert!(tool_names.contains(&"list_extensions"));
    assert!(tool_names.contains(&"list_extension_groups"));
    assert!(tool_names.contains(&"create_extension_group"));
    assert!(tool_names.contains(&"delete_extension"));
    assert!(tool_names.contains(&"delete_extension_group"));
    assert!(tool_names.contains(&"assign_extension_group_to_profile"));
    // Team lock tools
    assert!(tool_names.contains(&"get_team_locks"));
    assert!(tool_names.contains(&"get_team_lock_status"));
    assert!(tool_names.contains(&"get_sync_settings"));
    assert!(tool_names.contains(&"save_sync_settings"));
    assert!(tool_names.contains(&"request_profile_sync"));
    assert!(tool_names.contains(&"validate_recipe"));
    assert!(tool_names.contains(&"preview_recipe"));
    assert!(tool_names.contains(&"run_recipe"));
    assert!(tool_names.contains(&"run_batch_profile_workflow"));
    // Synchronizer tools
    assert!(tool_names.contains(&"start_sync_session"));
    assert!(tool_names.contains(&"stop_sync_session"));
    assert!(tool_names.contains(&"get_sync_sessions"));
    assert!(tool_names.contains(&"remove_sync_follower"));
    // Action recorder tools
    assert!(tool_names.contains(&"start_action_recording"));
    assert!(tool_names.contains(&"stop_action_recording"));
    assert!(tool_names.contains(&"get_recorded_events"));
    assert!(tool_names.contains(&"list_recordings"));
    assert!(tool_names.contains(&"export_recording_as_recipe"));
    assert!(tool_names.contains(&"replay_recording"));
    // Browser interaction tools
    assert!(tool_names.contains(&"navigate"));
    assert!(tool_names.contains(&"screenshot"));
    assert!(tool_names.contains(&"evaluate_javascript"));
    assert!(tool_names.contains(&"click_element"));
    assert!(tool_names.contains(&"type_text"));
    assert!(tool_names.contains(&"hover_element"));
    assert!(tool_names.contains(&"focus_element"));
    assert!(tool_names.contains(&"scroll_to"));
    assert!(tool_names.contains(&"scroll_element_into_view"));
    assert!(tool_names.contains(&"press_key"));
    assert!(tool_names.contains(&"press_hotkey"));
    assert!(tool_names.contains(&"select_option"));
    assert!(tool_names.contains(&"check_checkbox"));
    assert!(tool_names.contains(&"uncheck_checkbox"));
    assert!(tool_names.contains(&"element_exists"));
    assert!(tool_names.contains(&"get_element_text"));
    assert!(tool_names.contains(&"get_element_html"));
    assert!(tool_names.contains(&"get_element_attribute"));
    assert!(tool_names.contains(&"get_element_rect"));
    assert!(tool_names.contains(&"query_elements"));
    assert!(tool_names.contains(&"count_elements"));
    assert!(tool_names.contains(&"get_page_content"));
    assert!(tool_names.contains(&"get_page_info"));
    assert!(tool_names.contains(&"list_tabs"));
    assert!(tool_names.contains(&"switch_tab"));
    assert!(tool_names.contains(&"new_tab"));
    assert!(tool_names.contains(&"close_tab"));
    assert!(tool_names.contains(&"get_active_tab"));
    assert!(tool_names.contains(&"set_file_input"));
    assert!(tool_names.contains(&"upload_file"));
    assert!(tool_names.contains(&"download_file"));
    assert!(tool_names.contains(&"wait_for_download"));
    assert!(tool_names.contains(&"list_frames"));
    assert!(tool_names.contains(&"get_frame_tree"));
    assert!(tool_names.contains(&"find_frame"));
    // Console capture tools
    assert!(tool_names.contains(&"start_console_capture"));
    assert!(tool_names.contains(&"get_console_logs"));
    assert!(tool_names.contains(&"clear_console_logs"));
    // Network capture tools
    assert!(tool_names.contains(&"start_network_capture"));
    assert!(tool_names.contains(&"get_network_requests"));
    assert!(tool_names.contains(&"get_response_body"));
    assert!(tool_names.contains(&"clear_network_requests"));
    // Traffic stats tools
    assert!(tool_names.contains(&"get_all_traffic_snapshots"));
    assert!(tool_names.contains(&"get_traffic_stats_for_period"));
    assert!(tool_names.contains(&"clear_all_traffic_stats"));
    // Cookie tools
    assert!(tool_names.contains(&"read_profile_cookies"));
    assert!(tool_names.contains(&"copy_profile_cookies"));
    assert!(tool_names.contains(&"import_cookies_from_file"));
    assert!(tool_names.contains(&"export_profile_cookies"));
    // Storage tools
    assert!(tool_names.contains(&"get_local_storage"));
    assert!(tool_names.contains(&"set_local_storage"));
    assert!(tool_names.contains(&"get_session_storage"));
    assert!(tool_names.contains(&"clear_storage"));
    assert!(tool_names.contains(&"wait_for_selector"));
    assert!(tool_names.contains(&"wait_for_text"));
    assert!(tool_names.contains(&"wait_for_navigation"));
    // Fingerprint / GeoIP tools
    assert!(tool_names.contains(&"generate_sample_fingerprint"));
    assert!(tool_names.contains(&"is_geoip_database_available"));
    assert!(tool_names.contains(&"download_geoip_database"));
  }

  #[test]
  fn test_mcp_server_initial_state() {
    let server = McpServer::new();
    assert!(!server.is_running());
  }

  #[tokio::test]
  async fn test_console_capture_handlers_use_buffered_state() {
    let server = McpServer::new();
    let shared = ConsoleCaptureShared {
      logs: Arc::new(AsyncMutex::new(VecDeque::from(vec![
        ConsoleLogEntry {
          timestamp: Some(1.0),
          source: "console".to_string(),
          level: "log".to_string(),
          text: "first".to_string(),
          url: Some("https://example.com".to_string()),
          line_number: Some(1),
        },
        ConsoleLogEntry {
          timestamp: Some(2.0),
          source: "console".to_string(),
          level: "error".to_string(),
          text: "second".to_string(),
          url: Some("https://example.com/app.js".to_string()),
          line_number: Some(2),
        },
      ]))),
      running: Arc::new(AtomicBool::new(true)),
      last_error: Arc::new(AsyncMutex::new(None)),
    };

    {
      let mut inner = server.inner.lock().await;
      inner.console_captures.insert(
        "profile-1".to_string(),
        ConsoleCaptureState {
          shared: shared.clone(),
          shutdown_tx: None,
        },
      );
    }

    let logs = server
      .handle_get_console_logs(&serde_json::json!({
        "profile_id": "profile-1",
        "limit": 10
      }))
      .await
      .expect("console logs should be returned");
    let parsed = parse_text_tool_payload(logs);
    assert_eq!(parsed["total"], 2);
    assert_eq!(parsed["returned"], 2);
    assert_eq!(parsed["logs"][1]["level"], "error");

    let cleared = server
      .handle_clear_console_logs(&serde_json::json!({
        "profile_id": "profile-1"
      }))
      .await
      .expect("console logs should be cleared");
    let cleared_parsed = parse_text_tool_payload(cleared);
    assert_eq!(cleared_parsed["cleared"], 2);

    let logs_after_clear = server
      .handle_get_console_logs(&serde_json::json!({
        "profile_id": "profile-1"
      }))
      .await
      .expect("console logs should still be queryable");
    let parsed_after_clear = parse_text_tool_payload(logs_after_clear);
    assert_eq!(parsed_after_clear["total"], 0);
  }

  #[tokio::test]
  async fn test_network_capture_handlers_use_buffered_state() {
    let server = McpServer::new();
    let shared = NetworkCaptureShared {
      requests: Arc::new(AsyncMutex::new(VecDeque::from(vec![NetworkRequestEntry {
        request_id: "req-1".to_string(),
        url: "https://example.com/api".to_string(),
        method: "GET".to_string(),
        resource_type: Some("XHR".to_string()),
        initiator_type: Some("script".to_string()),
        start_timestamp: Some(1.0),
        end_timestamp: Some(1.2),
        duration_ms: Some(200.0),
        status: Some(200),
        status_text: Some("OK".to_string()),
        mime_type: Some("application/json".to_string()),
        request_headers: serde_json::json!({ "accept": "application/json" }),
        response_headers: serde_json::json!({ "content-type": "application/json" }),
        post_data: None,
        encoded_data_length: Some(128.0),
        failed: false,
        error_text: None,
        blocked_reason: None,
        has_response_body: true,
        response_body: None,
        response_body_base64_encoded: false,
      }]))),
      running: Arc::new(AtomicBool::new(true)),
      last_error: Arc::new(AsyncMutex::new(None)),
    };
    let (body_request_tx, _body_request_rx) = mpsc::channel(4);

    {
      let mut inner = server.inner.lock().await;
      inner.network_captures.insert(
        "profile-2".to_string(),
        NetworkCaptureState {
          shared: shared.clone(),
          shutdown_tx: None,
          body_request_tx,
        },
      );
    }

    let requests = server
      .handle_get_network_requests(&serde_json::json!({
        "profile_id": "profile-2",
        "limit": 10
      }))
      .await
      .expect("network requests should be returned");
    let parsed = parse_text_tool_payload(requests);
    assert_eq!(parsed["total"], 1);
    assert_eq!(parsed["requests"][0]["requestId"], "req-1");
    assert_eq!(parsed["requests"][0]["status"], 200);

    let cleared = server
      .handle_clear_network_requests(&serde_json::json!({
        "profile_id": "profile-2"
      }))
      .await
      .expect("network requests should be cleared");
    let cleared_parsed = parse_text_tool_payload(cleared);
    assert_eq!(cleared_parsed["cleared"], 1);

    let requests_after_clear = server
      .handle_get_network_requests(&serde_json::json!({
        "profile_id": "profile-2"
      }))
      .await
      .expect("network requests should still be queryable");
    let parsed_after_clear = parse_text_tool_payload(requests_after_clear);
    assert_eq!(parsed_after_clear["total"], 0);
  }

  #[tokio::test]
  async fn test_get_response_body_prefers_cached_capture_body() {
    let server = McpServer::new();
    let shared = NetworkCaptureShared {
      requests: Arc::new(AsyncMutex::new(VecDeque::from(vec![NetworkRequestEntry {
        request_id: "req-body-1".to_string(),
        url: "https://example.com/data.json".to_string(),
        method: "GET".to_string(),
        resource_type: Some("Fetch".to_string()),
        initiator_type: Some("script".to_string()),
        start_timestamp: Some(1.0),
        end_timestamp: Some(1.5),
        duration_ms: Some(500.0),
        status: Some(200),
        status_text: Some("OK".to_string()),
        mime_type: Some("application/json".to_string()),
        request_headers: serde_json::json!({}),
        response_headers: serde_json::json!({}),
        post_data: None,
        encoded_data_length: Some(32.0),
        failed: false,
        error_text: None,
        blocked_reason: None,
        has_response_body: true,
        response_body: Some("{\"message\":\"ok\"}".to_string()),
        response_body_base64_encoded: false,
      }]))),
      running: Arc::new(AtomicBool::new(true)),
      last_error: Arc::new(AsyncMutex::new(None)),
    };
    let (body_request_tx, _body_request_rx) = mpsc::channel(1);

    {
      let mut inner = server.inner.lock().await;
      inner.network_captures.insert(
        "profile-cache".to_string(),
        NetworkCaptureState {
          shared,
          shutdown_tx: None,
          body_request_tx,
        },
      );
    }

    let response = server
      .handle_get_response_body(&serde_json::json!({
        "profile_id": "profile-cache",
        "request_id": "req-body-1"
      }))
      .await
      .expect("cached response body should be returned without a live browser");
    let parsed = parse_text_tool_payload(response);
    assert_eq!(parsed["requestId"], "req-body-1");
    assert_eq!(parsed["base64Encoded"], false);
    assert_eq!(parsed["body"], "{\"message\":\"ok\"}");
    assert_eq!(parsed["byteLength"], 16);
  }

  #[tokio::test]
  async fn test_traffic_stats_handler_returns_null_for_unknown_profile() {
    let server = McpServer::new();
    let result = server
      .handle_get_traffic_stats_for_period(&serde_json::json!({
        "profile_id": "missing-profile",
        "seconds": 60
      }))
      .await
      .expect("traffic stats handler should succeed");

    let parsed = parse_text_tool_payload(result);
    assert_eq!(parsed["profileId"], "missing-profile");
    assert_eq!(parsed["seconds"], 60);
    assert!(parsed["stats"].is_null());
  }

  #[test]
  fn test_cookie_copy_args_support_camel_case_and_optional_name() {
    let args: CopyProfileCookiesArgs = McpServer::parse_arguments(&serde_json::json!({
      "sourceProfileId": "source-1",
      "targetProfileIds": ["target-1", "target-2"],
      "selectedCookies": [
        { "domain": ".example.com", "name": "sid" },
        { "domain": ".example.org" }
      ]
    }))
    .expect("cookie copy args should parse");

    assert_eq!(args.source_profile_id, "source-1");
    assert_eq!(args.target_profile_ids.len(), 2);
    assert_eq!(args.selected_cookies.len(), 2);
    assert_eq!(args.selected_cookies[0].name.as_deref(), Some("sid"));
    assert!(args.selected_cookies[1].name.is_none());
  }

  #[tokio::test]
  async fn test_cookie_handlers_require_app_handle_for_mutations() {
    let server = McpServer::new();

    let copy_error = server
      .handle_copy_profile_cookies(&serde_json::json!({
        "source_profile_id": "source-1",
        "target_profile_ids": ["target-1"]
      }))
      .await
      .expect_err("copy should require app handle");
    assert_eq!(copy_error.code, MCP_ERROR_INTERNAL);

    let import_error = server
      .handle_import_cookies_from_file(&serde_json::json!({
        "profile_id": "target-1",
        "content": "# Netscape HTTP Cookie File\n"
      }))
      .await
      .expect_err("import should require app handle");
    assert_eq!(import_error.code, MCP_ERROR_INTERNAL);
  }

  #[test]
  fn test_prepare_storage_value_distinguishes_string_and_json() {
    let (string_value, string_kind) =
      McpServer::prepare_storage_value(&serde_json::json!("hello")).expect("string should parse");
    assert_eq!(string_value, "hello");
    assert_eq!(string_kind, "string");

    let (json_value, json_kind) = McpServer::prepare_storage_value(&serde_json::json!({
      "enabled": true,
      "count": 2
    }))
    .expect("json should serialize");
    assert_eq!(json_kind, "json");
    assert_eq!(json_value, r#"{"count":2,"enabled":true}"#);
  }

  #[test]
  fn test_cookie_lock_errors_can_fallback_to_empty_download_cookie_header() {
    assert!(McpServer::can_fallback_to_empty_cookie_header(
      "Failed to prepare statement: database is locked"
    ));
    assert!(McpServer::can_fallback_to_empty_cookie_header(
      "resource temporarily unavailable"
    ));
    assert!(!McpServer::can_fallback_to_empty_cookie_header(
      "cookie database not found"
    ));
  }

  #[test]
  fn test_clear_storage_args_support_defaults() {
    let args: ClearStorageArgs = McpServer::parse_arguments(&serde_json::json!({
      "profileId": "profile-1"
    }))
    .expect("clear storage args should parse");

    assert_eq!(args.profile_id, "profile-1");
    assert!(args.storage.is_none());
    assert!(args.key.is_none());
  }

  #[test]
  fn test_generate_sample_fingerprint_args_parse_object_config() {
    let args: GenerateSampleFingerprintArgs = McpServer::parse_arguments(&serde_json::json!({
      "browser": "chromium",
      "version": "136.0.0.0",
      "config": {
        "os": "windows"
      }
    }))
    .expect("fingerprint args should parse");

    assert_eq!(args.browser, "chromium");
    assert_eq!(args.version, "136.0.0.0");
    assert_eq!(args.config["os"], "windows");
  }

  #[test]
  fn test_locator_read_args_support_timeout() {
    let args: LocatorReadArgs = McpServer::parse_arguments(&serde_json::json!({
      "profileId": "profile-1",
      "locator": {
        "by": "css",
        "value": "#status"
      },
      "auto_wait": false,
      "timeout_ms": 4321
    }))
    .expect("locator read args should parse");

    assert_eq!(args.profile_id, "profile-1");
    assert_eq!(args.options.auto_wait, Some(false));
    assert_eq!(args.options.timeout_ms, Some(4321));
  }

  #[test]
  fn test_locator_read_args_support_frame_selector() {
    let args: LocatorReadArgs = McpServer::parse_arguments(&serde_json::json!({
      "profileId": "profile-frame",
      "locator": {
        "by": "css",
        "value": "#iframe-result"
      },
      "frame": {
        "by": "index",
        "value": 1
      }
    }))
    .expect("locator read args should parse frame selector");

    assert_eq!(args.profile_id, "profile-frame");
    assert_eq!(
      args.frame.as_ref().map(|frame| frame.by),
      Some(McpFrameBy::Index)
    );
  }

  #[test]
  fn test_query_elements_args_support_timeout_and_limit() {
    let args: QueryElementsArgs = McpServer::parse_arguments(&serde_json::json!({
      "profileId": "profile-2",
      "locator": {
        "by": "text",
        "value": "Item"
      },
      "limit": 7,
      "auto_wait": false,
      "timeout_ms": 2500
    }))
    .expect("query args should parse");

    assert_eq!(args.profile_id, "profile-2");
    assert_eq!(args.limit, Some(7));
    assert_eq!(args.options.auto_wait, Some(false));
    assert_eq!(args.options.timeout_ms, Some(2500));
  }

  #[test]
  fn test_query_elements_args_support_frame_selector() {
    let args: QueryElementsArgs = McpServer::parse_arguments(&serde_json::json!({
      "profileId": "profile-3",
      "locator": {
        "by": "text",
        "value": "iframe-submitted"
      },
      "frame": {
        "by": "name",
        "value": "child-frame"
      }
    }))
    .expect("query args should parse frame selector");

    assert_eq!(args.profile_id, "profile-3");
    assert_eq!(
      args.frame.as_ref().map(|frame| frame.by),
      Some(McpFrameBy::Name)
    );
  }

  #[test]
  fn test_tab_selector_by_index_accepts_numeric_string() {
    let tabs = vec![
      McpTabNode {
        id: "tab-0".to_string(),
        index: 0,
        title: "Home".to_string(),
        url: "https://example.com".to_string(),
        target_type: "page".to_string(),
        active: false,
      },
      McpTabNode {
        id: "tab-1".to_string(),
        index: 1,
        title: "Docs".to_string(),
        url: "https://example.com/docs".to_string(),
        target_type: "page".to_string(),
        active: true,
      },
    ];
    let selector: McpTabRef = serde_json::from_value(serde_json::json!({
      "by": "index",
      "value": "1"
    }))
    .expect("tab selector should parse");

    let resolved = McpServer::resolve_tab_selector(&tabs, &selector)
      .expect("numeric string tab selector should resolve");
    assert_eq!(resolved.id, "tab-1");
  }

  #[test]
  fn test_save_sync_settings_args_support_camel_case() {
    let args: SaveSyncSettingsArgs = McpServer::parse_arguments(&serde_json::json!({
      "syncServerUrl": "https://sync.example.test",
      "syncToken": "token-123"
    }))
    .expect("sync settings args should parse");

    assert_eq!(
      args.sync_server_url.as_deref(),
      Some("https://sync.example.test")
    );
    assert_eq!(args.sync_token.as_deref(), Some("token-123"));
  }

  #[tokio::test]
  async fn test_extension_list_handlers_return_standard_text_content() {
    let server = McpServer::new();

    let extensions = server
      .handle_list_extensions()
      .await
      .expect("list extensions should return MCP text content");
    let extensions_payload = parse_text_tool_payload(extensions);
    assert!(
      extensions_payload.is_array(),
      "extensions payload should be a JSON array"
    );

    let groups = server
      .handle_list_extension_groups()
      .await
      .expect("list extension groups should return MCP text content");
    let groups_payload = parse_text_tool_payload(groups);
    assert!(
      groups_payload.is_array(),
      "extension groups payload should be a JSON array"
    );
  }

  #[test]
  fn test_validate_recipe_args_parse_recipe_payload() {
    let args: ValidateRecipeArgs = McpServer::parse_arguments(&serde_json::json!({
      "recipe": {
        "version": 1,
        "name": "demo",
        "defaults": {
          "timeoutMs": 2000,
          "autoWait": true
        },
        "steps": [{
          "id": "open",
          "tool": "navigate",
          "arguments": {
            "profileId": "profile-1",
            "url": "https://example.com"
          }
        }]
      }
    }))
    .expect("recipe args should parse");

    assert_eq!(args.recipe.version, 1);
    assert_eq!(args.recipe.name, "demo");
    assert_eq!(
      args
        .recipe
        .defaults
        .as_ref()
        .and_then(|value| value.timeout_ms),
      Some(2000)
    );
    assert_eq!(args.recipe.steps.len(), 1);
    assert_eq!(args.recipe.steps[0].tool, "navigate");
  }

  #[tokio::test]
  async fn test_validate_recipe_accepts_known_tools_and_reports_warnings() {
    let server = McpServer::new();
    let result = server
      .handle_validate_recipe(&serde_json::json!({
        "recipe": {
          "version": 1,
          "name": "demo",
          "steps": [
            {
              "id": "open",
              "tool": "navigate",
              "arguments": {
                "profile_id": "profile-1",
                "url": "https://example.com"
              },
              "retry": {
                "max_attempts": 2
              }
            }
          ]
        }
      }))
      .await
      .expect("recipe should validate");

    let parsed = parse_text_tool_payload(result);
    assert_eq!(parsed["valid"], true);
    assert_eq!(parsed["stepCount"], 1);
    assert_eq!(parsed["warnings"][0]["code"], "unsafe_retry_tool");
  }

  #[tokio::test]
  async fn test_validate_recipe_rejects_unknown_tool_and_missing_required_arguments() {
    let server = McpServer::new();

    let unknown_tool = server
      .handle_validate_recipe(&serde_json::json!({
        "recipe": {
          "version": 1,
          "name": "bad",
          "steps": [{
            "id": "x",
            "tool": "missing_tool",
            "arguments": {}
          }]
        }
      }))
      .await
      .expect_err("unknown tool should fail");
    assert_eq!(unknown_tool.code, MCP_ERROR_INVALID_PARAMS);

    let missing_args = server
      .handle_validate_recipe(&serde_json::json!({
        "recipe": {
          "version": 1,
          "name": "bad",
          "steps": [{
            "id": "x",
            "tool": "navigate",
            "arguments": {
              "url": "https://example.com"
            }
          }]
        }
      }))
      .await
      .expect_err("missing profile_id should fail");
    assert_eq!(missing_args.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[tokio::test]
  async fn test_preview_recipe_merges_defaults_and_exposes_replay_metadata() {
    let server = McpServer::new();
    let result = server
      .handle_preview_recipe(&serde_json::json!({
        "recipe": {
          "version": 1,
          "name": "preview-demo",
          "defaults": {
            "timeout_ms": 3210,
            "auto_wait": false
          },
          "steps": [
            {
              "id": "info",
              "tool": "get_page_info",
              "arguments": {
                "profileId": "profile-1"
              },
              "output": "page_info",
              "retry": {
                "maxAttempts": 2,
                "delayMs": 150
              }
            }
          ]
        }
      }))
      .await
      .expect("preview should succeed");

    let parsed = parse_text_tool_payload(result);
    assert_eq!(parsed["stepCount"], 1);
    assert_eq!(parsed["steps"][0]["retrySafe"], true);
    assert_eq!(
      parsed["steps"][0]["effectiveArguments"]["profileId"],
      "profile-1"
    );
    assert_eq!(parsed["steps"][0]["effectiveArguments"]["timeout_ms"], 3210);
    assert_eq!(parsed["steps"][0]["effectiveArguments"]["auto_wait"], false);
    assert_eq!(parsed["steps"][0]["replay"]["tool"], "get_page_info");
    assert_eq!(parsed["steps"][0]["output"], "page_info");
  }

  #[tokio::test]
  async fn test_run_recipe_executes_steps_and_records_replay() {
    let server = McpServer::new();
    let result = server
      .handle_run_recipe(&serde_json::json!({
        "recipe": {
          "version": 1,
          "name": "traffic-probe",
          "steps": [
            {
              "id": "probe",
              "tool": "get_traffic_stats_for_period",
              "arguments": {
                "profile_id": "{{inputs.profile_id}}",
                "seconds": "{{inputs.seconds}}"
              },
              "output": "probe_output"
            },
            {
              "id": "echo",
              "tool": "get_traffic_stats_for_period",
              "arguments": {
                "profile_id": "{{steps.probe.result.profileId}}",
                "seconds": "{{steps.probe.result.seconds}}"
              },
              "condition": "steps.probe.status",
              "output": "echo_output"
            }
          ]
        },
        "inputs": {
          "profile_id": "missing-profile",
          "seconds": 60
        }
      }))
      .await
      .expect("run_recipe should succeed");

    let parsed = parse_text_tool_payload(result);
    assert_eq!(parsed["status"], "success");
    assert_eq!(parsed["successfulSteps"], 2);
    assert_eq!(
      parsed["outputs"]["probe_output"]["profileId"],
      "missing-profile"
    );
    assert_eq!(parsed["outputs"]["echo_output"]["seconds"], 60);
    assert_eq!(
      parsed["steps"][1]["replay"]["arguments"]["profile_id"],
      "missing-profile"
    );
    assert_eq!(parsed["steps"][1]["condition"]["matched"], true);
  }

  #[tokio::test]
  async fn test_run_recipe_records_failure_and_stops_by_default() {
    let server = McpServer::new();
    let result = server
      .handle_run_recipe(&serde_json::json!({
        "recipe": {
          "version": 1,
          "name": "stop-on-failure",
          "steps": [
            {
              "id": "broken",
              "tool": "get_traffic_stats_for_period",
              "arguments": {
                "profile_id": "{{inputs.profile_id}}",
                "seconds": 30
              }
            },
            {
              "id": "never-runs",
              "tool": "get_all_traffic_snapshots",
              "arguments": {}
            }
          ]
        },
        "inputs": {}
      }))
      .await
      .expect("run_recipe should return structured failure report");

    let parsed = parse_text_tool_payload(result);
    assert_eq!(parsed["status"], "failed");
    assert_eq!(parsed["failedSteps"], 1);
    assert_eq!(parsed["completedSteps"], 1);
    assert_eq!(parsed["stoppedAtStepId"], "broken");
    assert_eq!(parsed["steps"][0]["status"], "failed");
    assert_eq!(
      parsed["steps"][0]["error"]["code"],
      MCP_ERROR_INVALID_PARAMS
    );
  }

  #[tokio::test]
  async fn test_run_recipe_can_continue_after_failure_and_use_failure_status_condition() {
    let server = McpServer::new();
    let result = server
      .handle_run_recipe(&serde_json::json!({
        "recipe": {
          "version": 1,
          "name": "continue-on-failure",
          "steps": [
            {
              "id": "broken",
              "tool": "get_traffic_stats_for_period",
              "arguments": {
                "profile_id": "{{inputs.profile_id}}",
                "seconds": 30
              },
              "on_error": "continue"
            },
            {
              "id": "fallback",
              "tool": "get_all_traffic_snapshots",
              "arguments": {},
              "condition": {
                "path": "steps.broken.status",
                "equals": "failed"
              }
            }
          ]
        },
        "inputs": {}
      }))
      .await
      .expect("run_recipe should continue after step failure");

    let parsed = parse_text_tool_payload(result);
    assert_eq!(parsed["status"], "completed_with_errors");
    assert_eq!(parsed["failedSteps"], 1);
    assert_eq!(parsed["successfulSteps"], 1);
    assert_eq!(parsed["steps"][0]["status"], "failed");
    assert_eq!(parsed["steps"][1]["status"], "success");
    assert_eq!(parsed["steps"][1]["condition"]["matched"], true);
  }

  #[test]
  fn test_run_batch_profile_workflow_args_support_snake_case() {
    let args: RunBatchProfileWorkflowArgs = McpServer::parse_arguments(&serde_json::json!({
      "profile_ids": ["profile-1", "profile-2"],
      "recipe": {
        "version": 1,
        "name": "batch",
        "steps": [{
          "id": "probe",
          "tool": "get_page_info",
          "arguments": {
            "profile_id": "{{inputs.profile_id}}"
          }
        }]
      },
      "inputs": {
        "seconds": 10
      },
      "max_concurrency": 2,
      "stop_on_profile_failure": true,
      "per_profile_inputs": {
        "profile-2": {
          "region": "cn"
        }
      }
    }))
    .expect("batch args should parse");

    assert_eq!(args.profile_ids.len(), 2);
    assert_eq!(args.max_concurrency, Some(2));
    assert_eq!(args.stop_on_profile_failure, Some(true));
    assert_eq!(
      args
        .per_profile_inputs
        .as_ref()
        .and_then(|value| value.get("profile-2"))
        .and_then(|value| value.get("region"))
        .and_then(|value| value.as_str()),
      Some("cn")
    );
  }

  #[tokio::test]
  async fn test_run_batch_profile_workflow_aggregates_profile_reports() {
    let server = McpServer::new();
    let result = server
      .handle_run_batch_profile_workflow(&serde_json::json!({
        "profile_ids": ["profile-a", "profile-b"],
        "recipe": {
          "version": 1,
          "name": "batch-probe",
          "steps": [{
            "id": "probe",
            "tool": "get_traffic_stats_for_period",
            "arguments": {
              "profile_id": "{{inputs.profile_id}}",
              "seconds": "{{inputs.seconds}}"
            },
            "output": "probe"
          }]
        },
        "inputs": {
          "seconds": 30
        },
        "max_concurrency": 2,
        "per_profile_inputs": {
          "profile-b": {
            "seconds": 45
          }
        }
      }))
      .await
      .expect("batch workflow should succeed");

    let parsed = parse_text_tool_payload(result);
    assert_eq!(parsed["status"], "success");
    assert_eq!(parsed["totalProfiles"], 2);
    assert_eq!(parsed["executedProfiles"], 2);
    assert_eq!(parsed["successfulProfiles"], 2);
    assert_eq!(parsed["results"][0]["profileId"], "profile-a");
    assert_eq!(
      parsed["results"][0]["report"]["outputs"]["probe"]["seconds"],
      30
    );
    assert_eq!(
      parsed["results"][1]["report"]["outputs"]["probe"]["seconds"],
      45
    );
  }

  #[tokio::test]
  async fn test_run_batch_profile_workflow_can_stop_after_first_failed_profile() {
    let server = McpServer::new();
    let result = server
      .handle_run_batch_profile_workflow(&serde_json::json!({
        "profile_ids": ["profile-a", "profile-b", "profile-c"],
        "recipe": {
          "version": 1,
          "name": "batch-stop",
          "steps": [{
            "id": "probe",
            "tool": "get_traffic_stats_for_period",
            "arguments": {
              "profile_id": "{{inputs.profile_id}}",
              "seconds": "{{inputs.seconds}}"
            }
          }]
        },
        "inputs": {},
        "stop_on_profile_failure": true
      }))
      .await
      .expect("batch workflow should return structured failure report");

    let parsed = parse_text_tool_payload(result);
    assert_eq!(parsed["status"], "failed");
    assert_eq!(parsed["executedProfiles"], 1);
    assert_eq!(parsed["failedProfiles"], 1);
    assert_eq!(parsed["stoppedAfterProfileId"], "profile-a");
    assert_eq!(parsed["skippedProfileIds"][0], "profile-b");
    assert_eq!(parsed["skippedProfileIds"][1], "profile-c");
  }

  #[tokio::test]
  async fn test_generate_sample_fingerprint_and_geoip_download_require_app_handle() {
    let server = McpServer::new();

    let fingerprint_error = server
      .handle_generate_sample_fingerprint(&serde_json::json!({
        "browser": "chromium",
        "version": "136.0.0.0",
        "config": {
          "os": "windows"
        }
      }))
      .await
      .expect_err("fingerprint generation should require app handle");
    assert_eq!(fingerprint_error.code, MCP_ERROR_INTERNAL);

    let geoip_error = server
      .handle_download_geoip_database()
      .await
      .expect_err("geoip download should require app handle");
    assert_eq!(geoip_error.code, MCP_ERROR_INTERNAL);
  }

  #[tokio::test]
  async fn test_wait_with_timeout_returns_value_and_times_out() {
    let server = McpServer::new();
    let attempts = Arc::new(AsyncMutex::new(0_u32));
    let attempts_for_wait = attempts.clone();

    let value = server
      .wait_with_timeout(
        1_000,
        || {
          let attempts = attempts_for_wait.clone();
          async move {
            let mut attempts = attempts.lock().await;
            *attempts += 1;
            if *attempts >= 2 {
              Ok(Some("ready"))
            } else {
              Ok(None)
            }
          }
        },
        || "should not timeout".to_string(),
      )
      .await
      .expect("wait should eventually succeed");
    assert_eq!(value, "ready");

    let timeout_error = server
      .wait_with_timeout(
        1_000,
        || async { Ok::<Option<()>, McpError>(None) },
        || "timed out".to_string(),
      )
      .await
      .expect_err("wait should timeout");
    assert_eq!(timeout_error.code, MCP_ERROR_TIMEOUT);
  }

  #[test]
  fn test_build_locator_condition_expression_serializes_locator() {
    let expression = McpServer::build_locator_condition_expression(
      &McpLocator {
        by: McpLocatorBy::Text,
        value: "hello".to_string(),
        nth: Some(1),
        exact: Some(true),
        name: None,
      },
      "visible",
    )
    .expect("locator expression should build");

    assert!(expression.contains("\"by\":\"text\""));
    assert!(expression.contains("\"value\":\"hello\""));
    assert!(expression.contains("\"nth\":1"));
    assert!(expression.contains("\"exact\":true"));
  }

  #[test]
  fn test_locator_parse_and_validation_follow_schema() {
    let locator: McpLocator = serde_json::from_value(serde_json::json!({
      "by": "role",
      "value": "button",
      "name": "登录",
      "exact": true,
      "nth": 2
    }))
    .expect("locator should parse from schema");
    locator.validate().expect("locator should validate");
    assert_eq!(locator.by, McpLocatorBy::Role);
    assert_eq!(locator.name.as_deref(), Some("登录"));
    assert_eq!(locator.nth, Some(2));

    let invalid = McpLocator {
      by: McpLocatorBy::Css,
      value: "   ".to_string(),
      nth: None,
      exact: None,
      name: None,
    }
    .validate()
    .expect_err("blank locator value should be rejected");
    assert_eq!(invalid.code, MCP_ERROR_INVALID_PARAMS);

    let invalid_role_name = McpLocator {
      by: McpLocatorBy::Role,
      value: "button".to_string(),
      nth: None,
      exact: None,
      name: Some("   ".to_string()),
    }
    .validate()
    .expect_err("blank role locator name should be rejected");
    assert_eq!(invalid_role_name.code, MCP_ERROR_INVALID_PARAMS);

    let label_locator: McpLocator = serde_json::from_value(serde_json::json!({
      "by": "label",
      "value": "邮箱",
      "exact": false
    }))
    .expect("label locator should parse from schema");
    label_locator
      .validate()
      .expect("label locator should validate");
    assert_eq!(label_locator.by, McpLocatorBy::Label);

    let placeholder_locator: McpLocator = serde_json::from_value(serde_json::json!({
      "by": "placeholder",
      "value": "Search"
    }))
    .expect("placeholder locator should parse from schema");
    placeholder_locator
      .validate()
      .expect("placeholder locator should validate");
    assert_eq!(placeholder_locator.by, McpLocatorBy::Placeholder);
  }

  #[test]
  fn test_frame_ref_parse_and_validation_follow_schema() {
    let main_frame: McpFrameRef = serde_json::from_value(serde_json::json!({
      "by": "main"
    }))
    .expect("main frame selector should parse");
    main_frame.validate().expect("main frame should validate");
    assert_eq!(main_frame.by, McpFrameBy::Main);

    let index_frame: McpFrameRef = serde_json::from_value(serde_json::json!({
      "by": "index",
      "value": 2
    }))
    .expect("index frame selector should parse");
    index_frame.validate().expect("index frame should validate");
    assert_eq!(
      index_frame.value,
      Some(McpFrameValue::Index(2)),
      "index value should remain typed"
    );

    let url_frame: McpFrameRef = serde_json::from_value(serde_json::json!({
      "by": "url",
      "value": "https://example.com/embed"
    }))
    .expect("url frame selector should parse");
    url_frame.validate().expect("url frame should validate");
    assert_eq!(url_frame.by, McpFrameBy::Url);
  }

  #[test]
  fn test_frame_ref_validation_rejects_invalid_value_shapes() {
    let main_with_value = McpFrameRef {
      by: McpFrameBy::Main,
      value: Some(McpFrameValue::Text("extra".to_string())),
    }
    .validate()
    .expect_err("main frame with value should fail");
    assert_eq!(main_with_value.code, MCP_ERROR_INVALID_PARAMS);

    let index_with_text = McpFrameRef {
      by: McpFrameBy::Index,
      value: Some(McpFrameValue::Text("1".to_string())),
    }
    .validate()
    .expect_err("index frame with string value should fail");
    assert_eq!(index_with_text.code, MCP_ERROR_INVALID_PARAMS);

    let blank_name = McpFrameRef {
      by: McpFrameBy::Name,
      value: Some(McpFrameValue::Text("   ".to_string())),
    }
    .validate()
    .expect_err("blank frame name should fail");
    assert_eq!(blank_name.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[test]
  fn test_frame_ref_schema_supports_main_index_url_name_id() {
    let schema = McpServer::frame_ref_input_schema("Frame selector");
    let by_enum = schema
      .get("properties")
      .and_then(|properties| properties.get("by"))
      .and_then(|by| by.get("enum"))
      .and_then(|values| values.as_array())
      .expect("frame schema enum should exist");

    let values: Vec<&str> = by_enum.iter().filter_map(|value| value.as_str()).collect();
    assert_eq!(values, vec!["main", "index", "url", "name", "id"]);
    assert!(
      schema
        .get("properties")
        .and_then(|properties| properties.get("value"))
        .and_then(|value| value.get("oneOf"))
        .is_some(),
      "frame schema should allow integer or string value"
    );
  }

  #[test]
  fn test_frame_ref_description_and_tree_shapes_are_stable() {
    let selector = McpFrameRef {
      by: McpFrameBy::Id,
      value: Some(McpFrameValue::Text("frame-123".to_string())),
    };
    assert_eq!(selector.describe(), "frame id frame-123");

    let tree = McpFrameTreeNode {
      id: "root".to_string(),
      parent_id: None,
      index: 0,
      depth: 0,
      url: "https://example.com".to_string(),
      name: Some("main".to_string()),
      origin: Some("https://example.com".to_string()),
      main: true,
      children: vec![McpFrameTreeNode {
        id: "child".to_string(),
        parent_id: Some("root".to_string()),
        index: 1,
        depth: 1,
        url: "https://example.com/embed".to_string(),
        name: Some("widget".to_string()),
        origin: Some("https://example.com".to_string()),
        main: false,
        children: vec![],
      }],
    };

    let serialized = serde_json::to_value(&tree).expect("frame tree should serialize");
    assert_eq!(serialized["children"][0]["depth"], 1);
    assert_eq!(serialized["children"][0]["main"], false);
  }

  #[test]
  fn test_parse_cdp_frame_tree_builds_nested_tree_with_preorder_indexes() {
    let tree = McpServer::parse_cdp_frame_tree(&serde_json::json!({
      "frameTree": {
        "frame": {
          "id": "root",
          "url": "https://example.com",
          "name": "",
          "securityOrigin": "https://example.com"
        },
        "childFrames": [
          {
            "frame": {
              "id": "child-a",
              "parentId": "root",
              "url": "https://example.com/a",
              "name": "frame-a",
              "securityOrigin": "https://example.com"
            }
          },
          {
            "frame": {
              "id": "child-b",
              "parentId": "root",
              "url": "https://cdn.example.net/widget",
              "name": "widget",
              "securityOrigin": "https://cdn.example.net"
            },
            "childFrames": [
              {
                "frame": {
                  "id": "grandchild",
                  "parentId": "child-b",
                  "url": "https://payments.example.org/embedded",
                  "name": "payment",
                  "securityOrigin": "https://payments.example.org"
                }
              }
            ]
          }
        ]
      }
    }))
    .expect("frame tree should parse");

    assert!(tree.main);
    assert_eq!(tree.index, 0);
    assert_eq!(tree.children.len(), 2);
    assert_eq!(tree.children[0].index, 1);
    assert_eq!(tree.children[1].index, 2);
    assert_eq!(tree.children[1].children[0].index, 3);
    assert_eq!(tree.children[1].children[0].depth, 2);
    assert_eq!(tree.children[1].name.as_deref(), Some("widget"));
  }

  #[test]
  fn test_flatten_frame_tree_returns_preorder_nodes_with_child_counts() {
    let tree = McpFrameTreeNode {
      id: "root".to_string(),
      parent_id: None,
      index: 0,
      depth: 0,
      url: "https://example.com".to_string(),
      name: None,
      origin: Some("https://example.com".to_string()),
      main: true,
      children: vec![
        McpFrameTreeNode {
          id: "child-a".to_string(),
          parent_id: Some("root".to_string()),
          index: 1,
          depth: 1,
          url: "https://example.com/a".to_string(),
          name: Some("a".to_string()),
          origin: Some("https://example.com".to_string()),
          main: false,
          children: vec![],
        },
        McpFrameTreeNode {
          id: "child-b".to_string(),
          parent_id: Some("root".to_string()),
          index: 2,
          depth: 1,
          url: "https://cdn.example.net/widget".to_string(),
          name: Some("widget".to_string()),
          origin: Some("https://cdn.example.net".to_string()),
          main: false,
          children: vec![McpFrameTreeNode {
            id: "grandchild".to_string(),
            parent_id: Some("child-b".to_string()),
            index: 3,
            depth: 2,
            url: "https://payments.example.org/embedded".to_string(),
            name: Some("payment".to_string()),
            origin: Some("https://payments.example.org".to_string()),
            main: false,
            children: vec![],
          }],
        },
      ],
    };

    let frames = McpServer::flatten_frame_tree(&tree);
    assert_eq!(frames.len(), 4);
    assert_eq!(frames[0].id, "root");
    assert_eq!(frames[0].child_count, 2);
    assert_eq!(frames[1].id, "child-a");
    assert_eq!(frames[2].id, "child-b");
    assert_eq!(frames[2].child_count, 1);
    assert_eq!(frames[3].depth, 2);
  }

  #[test]
  fn test_parse_cdp_frame_tree_rejects_missing_root_frame() {
    let error = McpServer::parse_cdp_frame_tree(&serde_json::json!({
      "frameTree": {
        "childFrames": []
      }
    }))
    .expect_err("missing frame payload should fail");
    assert_eq!(error.code, MCP_ERROR_INTERNAL);
  }

  #[test]
  fn test_find_frame_args_parse_frame_selector() {
    let args: FindFrameArgs = McpServer::parse_arguments(&serde_json::json!({
      "profileId": "profile-1",
      "frame": {
        "by": "index",
        "value": 2
      }
    }))
    .expect("find frame args should parse");

    assert_eq!(args.profile_id, "profile-1");
    assert_eq!(args.frame.by, McpFrameBy::Index);
    assert_eq!(args.frame.value, Some(McpFrameValue::Index(2)));
  }

  #[test]
  fn test_switch_and_close_tab_args_parse_tab_selector() {
    let switch_args: SwitchTabArgs = McpServer::parse_arguments(&serde_json::json!({
      "profileId": "profile-1",
      "tab": {
        "by": "index",
        "value": 1
      }
    }))
    .expect("switch tab args should parse");
    assert_eq!(switch_args.profile_id, "profile-1");
    assert_eq!(switch_args.tab.by, McpTabBy::Index);
    assert_eq!(switch_args.tab.value, Some(McpTabValue::Index(1)));

    let close_args: CloseTabArgs = McpServer::parse_arguments(&serde_json::json!({
      "profileId": "profile-1",
      "tab": {
        "by": "title",
        "value": "Checkout"
      }
    }))
    .expect("close tab args should parse");
    assert_eq!(close_args.profile_id, "profile-1");
    assert_eq!(
      close_args.tab,
      Some(McpTabRef {
        by: McpTabBy::Title,
        value: Some(McpTabValue::Text("Checkout".to_string())),
      })
    );
  }

  #[test]
  fn test_evaluate_javascript_args_support_optional_frame() {
    let args: EvaluateJavascriptArgs = McpServer::parse_arguments(&serde_json::json!({
      "profileId": "profile-1",
      "expression": "document.title",
      "frame": {
        "by": "name",
        "value": "widget"
      },
      "await_promise": true,
      "wait_for_load": false
    }))
    .expect("evaluate args should parse");

    assert_eq!(args.profile_id, "profile-1");
    assert_eq!(args.expression, "document.title");
    assert_eq!(args.await_promise, Some(true));
    assert_eq!(args.wait_for_load, Some(false));
    assert_eq!(
      args.frame,
      Some(McpFrameRef {
        by: McpFrameBy::Name,
        value: Some(McpFrameValue::Text("widget".to_string())),
      })
    );
  }

  #[test]
  fn test_resolve_frame_selector_supports_main_index_id_name_and_url() {
    let frames = vec![
      McpFrameNode {
        id: "root".to_string(),
        parent_id: None,
        index: 0,
        depth: 0,
        url: "https://example.com".to_string(),
        name: None,
        origin: Some("https://example.com".to_string()),
        main: true,
        child_count: 2,
      },
      McpFrameNode {
        id: "child-a".to_string(),
        parent_id: Some("root".to_string()),
        index: 1,
        depth: 1,
        url: "https://example.com/a".to_string(),
        name: Some("login-frame".to_string()),
        origin: Some("https://example.com".to_string()),
        main: false,
        child_count: 0,
      },
      McpFrameNode {
        id: "child-b".to_string(),
        parent_id: Some("root".to_string()),
        index: 2,
        depth: 1,
        url: "https://cdn.example.net/widget".to_string(),
        name: Some("widget".to_string()),
        origin: Some("https://cdn.example.net".to_string()),
        main: false,
        child_count: 0,
      },
    ];

    let main = McpServer::resolve_frame_selector(
      &frames,
      &McpFrameRef {
        by: McpFrameBy::Main,
        value: None,
      },
    )
    .expect("main frame should resolve");
    assert_eq!(main.id, "root");

    let by_index = McpServer::resolve_frame_selector(
      &frames,
      &McpFrameRef {
        by: McpFrameBy::Index,
        value: Some(McpFrameValue::Index(2)),
      },
    )
    .expect("index frame should resolve");
    assert_eq!(by_index.id, "child-b");

    let by_id = McpServer::resolve_frame_selector(
      &frames,
      &McpFrameRef {
        by: McpFrameBy::Id,
        value: Some(McpFrameValue::Text("child-a".to_string())),
      },
    )
    .expect("id frame should resolve");
    assert_eq!(by_id.index, 1);

    let by_name = McpServer::resolve_frame_selector(
      &frames,
      &McpFrameRef {
        by: McpFrameBy::Name,
        value: Some(McpFrameValue::Text("widget".to_string())),
      },
    )
    .expect("name frame should resolve");
    assert_eq!(by_name.id, "child-b");

    let by_url = McpServer::resolve_frame_selector(
      &frames,
      &McpFrameRef {
        by: McpFrameBy::Url,
        value: Some(McpFrameValue::Text("cdn.example.net".to_string())),
      },
    )
    .expect("url frame should resolve");
    assert_eq!(by_url.id, "child-b");
  }

  #[test]
  fn test_resolve_frame_selector_rejects_ambiguous_or_missing_matches() {
    let frames = vec![
      McpFrameNode {
        id: "child-a".to_string(),
        parent_id: Some("root".to_string()),
        index: 1,
        depth: 1,
        url: "https://cdn.example.net/widget/a".to_string(),
        name: Some("widget".to_string()),
        origin: Some("https://cdn.example.net".to_string()),
        main: false,
        child_count: 0,
      },
      McpFrameNode {
        id: "child-b".to_string(),
        parent_id: Some("root".to_string()),
        index: 2,
        depth: 1,
        url: "https://cdn.example.net/widget/b".to_string(),
        name: Some("widget".to_string()),
        origin: Some("https://cdn.example.net".to_string()),
        main: false,
        child_count: 0,
      },
    ];

    let ambiguous = McpServer::resolve_frame_selector(
      &frames,
      &McpFrameRef {
        by: McpFrameBy::Name,
        value: Some(McpFrameValue::Text("widget".to_string())),
      },
    )
    .expect_err("ambiguous name should fail");
    assert_eq!(ambiguous.code, MCP_ERROR_INVALID_PARAMS);

    let missing = McpServer::resolve_frame_selector(
      &frames,
      &McpFrameRef {
        by: McpFrameBy::Id,
        value: Some(McpFrameValue::Text("missing".to_string())),
      },
    )
    .expect_err("missing id should fail");
    assert_eq!(missing.code, MCP_ERROR_NOT_FOUND);
  }

  #[test]
  fn test_cross_origin_frame_scenario_preserves_origin_and_targetability() {
    let tree = McpServer::parse_cdp_frame_tree(&serde_json::json!({
      "frameTree": {
        "frame": {
          "id": "root",
          "url": "http://127.0.0.1:4010/parent.html",
          "name": "",
          "securityOrigin": "http://127.0.0.1:4010"
        },
        "childFrames": [
          {
            "frame": {
              "id": "payment-frame",
              "parentId": "root",
              "url": "http://127.0.0.1:4011/checkout.html",
              "name": "checkout",
              "securityOrigin": "http://127.0.0.1:4011"
            }
          }
        ]
      }
    }))
    .expect("cross-origin frame tree should parse");

    let frames = McpServer::flatten_frame_tree(&tree);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].origin.as_deref(), Some("http://127.0.0.1:4010"));
    assert_eq!(frames[1].origin.as_deref(), Some("http://127.0.0.1:4011"));
    assert_ne!(
      frames[0].origin, frames[1].origin,
      "child iframe should remain cross-origin"
    );

    let resolved_by_name = McpServer::resolve_frame_selector(
      &frames,
      &McpFrameRef {
        by: McpFrameBy::Name,
        value: Some(McpFrameValue::Text("checkout".to_string())),
      },
    )
    .expect("cross-origin iframe should still be targetable by name");
    assert_eq!(resolved_by_name.id, "payment-frame");

    let resolved_by_url = McpServer::resolve_frame_selector(
      &frames,
      &McpFrameRef {
        by: McpFrameBy::Url,
        value: Some(McpFrameValue::Text("127.0.0.1:4011".to_string())),
      },
    )
    .expect("cross-origin iframe should still be targetable by url");
    assert_eq!(
      resolved_by_url.origin.as_deref(),
      Some("http://127.0.0.1:4011")
    );
  }

  #[test]
  fn test_tab_ref_parse_validation_and_resolution_are_stable() {
    let selector: McpTabRef = serde_json::from_value(serde_json::json!({
      "by": "url",
      "value": "checkout"
    }))
    .expect("tab selector should parse");
    selector.validate().expect("tab selector should validate");
    assert_eq!(
      selector,
      McpTabRef {
        by: McpTabBy::Url,
        value: Some(McpTabValue::Text("checkout".to_string())),
      }
    );

    let tabs = vec![
      McpTabNode {
        id: "tab-1".to_string(),
        index: 0,
        title: "Dashboard".to_string(),
        url: "https://example.com/dashboard".to_string(),
        target_type: "page".to_string(),
        active: true,
      },
      McpTabNode {
        id: "tab-2".to_string(),
        index: 1,
        title: "Checkout".to_string(),
        url: "https://pay.example.com/checkout".to_string(),
        target_type: "page".to_string(),
        active: false,
      },
    ];

    let resolved =
      McpServer::resolve_tab_selector(&tabs, &selector).expect("url selector should resolve tab");
    assert_eq!(resolved.id, "tab-2");

    let blank_error = McpTabRef {
      by: McpTabBy::Title,
      value: Some(McpTabValue::Text("   ".to_string())),
    }
    .validate()
    .expect_err("blank tab selector should fail");
    assert_eq!(blank_error.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[test]
  fn test_parse_execution_context_id_accepts_integer_payload() {
    let context_id = McpServer::parse_execution_context_id(&serde_json::json!({
      "executionContextId": 42
    }))
    .expect("context id should parse");
    assert_eq!(context_id, 42);

    let missing = McpServer::parse_execution_context_id(&serde_json::json!({}))
      .expect_err("missing context id should fail");
    assert_eq!(missing.code, MCP_ERROR_INTERNAL);
  }

  #[test]
  fn test_evaluate_javascript_schema_exposes_frame_selector() {
    let server = McpServer::new();
    let tools = server.get_tools();
    let tool = tools
      .iter()
      .find(|tool| tool.name == "evaluate_javascript")
      .expect("evaluate_javascript should exist");

    assert!(
      tool
        .input_schema
        .get("properties")
        .and_then(|properties| properties.get("frame"))
        .is_some(),
      "evaluate_javascript should expose frame selector"
    );
  }

  #[test]
  fn test_switch_and_close_tab_schema_expose_tab_selector() {
    let server = McpServer::new();
    let tools = server.get_tools();

    for tool_name in ["switch_tab", "close_tab"] {
      let tool = tools
        .iter()
        .find(|tool| tool.name == tool_name)
        .expect("tab management tool should exist");
      assert!(
        tool
          .input_schema
          .get("properties")
          .and_then(|properties| properties.get("tab"))
          .is_some(),
        "{tool_name} should expose tab selector"
      );
    }
  }

  #[test]
  fn test_frame_enabled_tools_expose_frame_selector() {
    let server = McpServer::new();
    let tools = server.get_tools();

    for tool_name in [
      "click_element",
      "type_text",
      "hover_element",
      "focus_element",
      "scroll_to",
      "scroll_element_into_view",
      "press_key",
      "press_hotkey",
      "select_option",
      "check_checkbox",
      "uncheck_checkbox",
      "get_element_html",
      "get_element_attribute",
      "get_element_rect",
      "set_file_input",
      "upload_file",
      "get_page_content",
      "wait_for_selector",
    ] {
      let tool = tools
        .iter()
        .find(|tool| tool.name == tool_name)
        .expect("tool should exist");
      assert!(
        tool
          .input_schema
          .get("properties")
          .and_then(|properties| properties.get("frame"))
          .is_some(),
        "{tool_name} should expose frame selector"
      );
    }
  }

  #[tokio::test]
  async fn test_evaluate_javascript_rejects_invalid_frame_before_profile_lookup() {
    let server = McpServer::new();
    let error = server
      .handle_evaluate_javascript(&serde_json::json!({
        "profile_id": "missing-profile",
        "expression": "1 + 1",
        "frame": {
          "by": "main",
          "value": "unexpected"
        }
      }))
      .await
      .expect_err("invalid frame selector should fail");
    assert_eq!(error.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[tokio::test]
  async fn test_click_type_content_and_wait_reject_invalid_frame_before_profile_lookup() {
    let server = McpServer::new();

    let click_error = server
      .handle_click_element(&serde_json::json!({
        "profile_id": "missing-profile",
        "selector": "#submit",
        "frame": {
          "by": "main",
          "value": "unexpected"
        }
      }))
      .await
      .expect_err("invalid click frame should fail");
    assert_eq!(click_error.code, MCP_ERROR_INVALID_PARAMS);

    let type_error = server
      .handle_type_text(&serde_json::json!({
        "profile_id": "missing-profile",
        "selector": "#email",
        "text": "hello",
        "frame": {
          "by": "main",
          "value": "unexpected"
        }
      }))
      .await
      .expect_err("invalid type frame should fail");
    assert_eq!(type_error.code, MCP_ERROR_INVALID_PARAMS);

    let content_error = server
      .handle_get_page_content(&serde_json::json!({
        "profile_id": "missing-profile",
        "frame": {
          "by": "index",
          "value": "not-an-int"
        }
      }))
      .await
      .expect_err("invalid content frame should fail");
    assert_eq!(content_error.code, MCP_ERROR_INVALID_PARAMS);

    let wait_error = server
      .handle_wait_for_selector(&serde_json::json!({
        "profile_id": "missing-profile",
        "locator": {
          "by": "css",
          "value": "#app"
        },
        "frame": {
          "by": "main",
          "value": "unexpected"
        }
      }))
      .await
      .expect_err("invalid wait frame should fail");
    assert_eq!(wait_error.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[tokio::test]
  async fn test_interaction_enhancement_tools_reject_invalid_inputs_before_profile_lookup() {
    let server = McpServer::new();

    let hover_error = server
      .handle_hover_element(&serde_json::json!({
        "profile_id": "missing-profile",
        "selector": "#menu",
        "frame": { "by": "main", "value": "unexpected" }
      }))
      .await
      .expect_err("invalid hover frame should fail");
    assert_eq!(hover_error.code, MCP_ERROR_INVALID_PARAMS);

    let focus_error = server
      .handle_focus_element(&serde_json::json!({
        "profile_id": "missing-profile",
        "selector": "#email",
        "frame": { "by": "main", "value": "unexpected" }
      }))
      .await
      .expect_err("invalid focus frame should fail");
    assert_eq!(focus_error.code, MCP_ERROR_INVALID_PARAMS);

    let scroll_error = server
      .handle_scroll_to(&serde_json::json!({
        "profile_id": "missing-profile",
        "behavior": "sideways"
      }))
      .await
      .expect_err("invalid scroll behavior should fail");
    assert_eq!(scroll_error.code, MCP_ERROR_INVALID_PARAMS);

    let press_key_error = server
      .handle_press_key(&serde_json::json!({
        "profile_id": "missing-profile",
        "key": "   "
      }))
      .await
      .expect_err("blank key should fail");
    assert_eq!(press_key_error.code, MCP_ERROR_INVALID_PARAMS);

    let press_hotkey_error = server
      .handle_press_hotkey(&serde_json::json!({
        "profile_id": "missing-profile",
        "keys": []
      }))
      .await
      .expect_err("empty hotkey should fail");
    assert_eq!(press_hotkey_error.code, MCP_ERROR_INVALID_PARAMS);

    let select_error = server
      .handle_select_option(&serde_json::json!({
        "profile_id": "missing-profile",
        "selector": "#country"
      }))
      .await
      .expect_err("missing select target choice should fail");
    assert_eq!(select_error.code, MCP_ERROR_INVALID_PARAMS);

    let check_error = server
      .handle_check_checkbox(&serde_json::json!({
        "profile_id": "missing-profile",
        "selector": "#tos",
        "frame": { "by": "main", "value": "unexpected" }
      }))
      .await
      .expect_err("invalid check frame should fail");
    assert_eq!(check_error.code, MCP_ERROR_INVALID_PARAMS);

    let uncheck_error = server
      .handle_uncheck_checkbox(&serde_json::json!({
        "profile_id": "missing-profile",
        "selector": "#tos",
        "frame": { "by": "main", "value": "unexpected" }
      }))
      .await
      .expect_err("invalid uncheck frame should fail");
    assert_eq!(uncheck_error.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[tokio::test]
  async fn test_dom_read_enhancement_tools_reject_invalid_inputs_before_profile_lookup() {
    let server = McpServer::new();

    let html_error = server
      .handle_get_element_html(&serde_json::json!({
        "profile_id": "missing-profile",
        "locator": {
          "by": "css",
          "value": "   "
        }
      }))
      .await
      .expect_err("blank html locator should fail");
    assert_eq!(html_error.code, MCP_ERROR_INVALID_PARAMS);

    let attribute_error = server
      .handle_get_element_attribute(&serde_json::json!({
        "profile_id": "missing-profile",
        "locator": {
          "by": "css",
          "value": "#link"
        },
        "name": "   "
      }))
      .await
      .expect_err("blank attribute name should fail");
    assert_eq!(attribute_error.code, MCP_ERROR_INVALID_PARAMS);

    let rect_error = server
      .handle_get_element_rect(&serde_json::json!({
        "profile_id": "missing-profile",
        "locator": {
          "by": "css",
          "value": "#box"
        },
        "frame": {
          "by": "main",
          "value": "unexpected"
        }
      }))
      .await
      .expect_err("invalid rect frame should fail");
    assert_eq!(rect_error.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[tokio::test]
  async fn test_role_locator_rejects_blank_name_before_profile_lookup() {
    let server = McpServer::new();
    let error = server
      .handle_wait_for_selector(&serde_json::json!({
        "profile_id": "missing-profile",
        "locator": {
          "by": "role",
          "value": "button",
          "name": "   "
        }
      }))
      .await
      .expect_err("blank role locator name should fail");
    assert_eq!(error.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[tokio::test]
  async fn test_tab_management_tools_reject_invalid_inputs_before_profile_lookup() {
    let server = McpServer::new();

    let switch_error = server
      .handle_switch_tab(&serde_json::json!({
        "profile_id": "missing-profile",
        "tab": {
          "by": "index",
          "value": "wrong-type"
        }
      }))
      .await
      .expect_err("invalid switch tab selector should fail");
    assert_eq!(switch_error.code, MCP_ERROR_INVALID_PARAMS);

    let new_tab_error = server
      .handle_new_tab(&serde_json::json!({
        "profile_id": "missing-profile",
        "url": "   "
      }))
      .await
      .expect_err("blank new tab url should fail");
    assert_eq!(new_tab_error.code, MCP_ERROR_INVALID_PARAMS);

    let close_error = server
      .handle_close_tab(&serde_json::json!({
        "profile_id": "missing-profile",
        "tab": {
          "by": "title",
          "value": "   "
        }
      }))
      .await
      .expect_err("blank close tab selector should fail");
    assert_eq!(close_error.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[tokio::test]
  async fn test_file_tools_reject_invalid_inputs_before_profile_lookup() {
    let server = McpServer::new();

    let set_error = server
      .handle_set_file_input(&serde_json::json!({
        "profile_id": "missing-profile",
        "selector": "#upload",
        "files": []
      }))
      .await
      .expect_err("empty file list should fail");
    assert_eq!(set_error.code, MCP_ERROR_INVALID_PARAMS);

    let upload_error = server
      .handle_upload_file(&serde_json::json!({
        "profile_id": "missing-profile",
        "locator": {
          "by": "css",
          "value": "   "
        },
        "files": ["/tmp/example.txt"]
      }))
      .await
      .expect_err("blank upload locator should fail");
    assert_eq!(upload_error.code, MCP_ERROR_INVALID_PARAMS);

    let download_error = server
      .handle_download_file(&serde_json::json!({
        "profile_id": "missing-profile",
        "url": "   "
      }))
      .await
      .expect_err("blank download url should fail");
    assert_eq!(download_error.code, MCP_ERROR_INVALID_PARAMS);

    let wait_error = server
      .handle_wait_for_download(&serde_json::json!({
        "profile_id": "missing-profile",
        "download_id": "   "
      }))
      .await
      .expect_err("blank download id should fail");
    assert_eq!(wait_error.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[tokio::test]
  async fn test_sync_settings_tools_require_app_handle_and_validate_inputs() {
    let server = McpServer::new();

    let get_error = server
      .handle_get_sync_settings()
      .await
      .expect_err("get sync settings should require app handle");
    assert_eq!(get_error.code, MCP_ERROR_INTERNAL);

    let save_blank_url_error = server
      .handle_save_sync_settings(&serde_json::json!({
        "sync_server_url": "   "
      }))
      .await
      .expect_err("blank sync url should fail");
    assert_eq!(save_blank_url_error.code, MCP_ERROR_INVALID_PARAMS);

    let save_blank_token_error = server
      .handle_save_sync_settings(&serde_json::json!({
        "sync_token": "   "
      }))
      .await
      .expect_err("blank sync token should fail");
    assert_eq!(save_blank_token_error.code, MCP_ERROR_INVALID_PARAMS);

    let request_blank_profile_error = server
      .handle_request_profile_sync(&serde_json::json!({
        "profile_id": "   "
      }))
      .await
      .expect_err("blank profile id should fail");
    assert_eq!(request_blank_profile_error.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[test]
  fn test_build_locator_resolution_expression_serializes_unified_snapshot_shape() {
    let expression = McpServer::build_locator_resolution_expression(&McpLocator {
      by: McpLocatorBy::Xpath,
      value: "//button".to_string(),
      nth: None,
      exact: None,
      name: None,
    })
    .expect("locator resolution expression should build");

    assert!(expression.contains("resolveLocatorSnapshot"));
    assert!(expression.contains("\"by\":\"xpath\""));
    assert!(expression.contains("matchedTexts"));
  }

  #[test]
  fn test_role_locator_schema_and_expression_include_name_filter() {
    let schema = McpServer::locator_input_schema("Locator");
    let by_enum = schema
      .get("properties")
      .and_then(|properties| properties.get("by"))
      .and_then(|by| by.get("enum"))
      .and_then(|value| value.as_array())
      .expect("locator by enum should exist");
    assert!(by_enum.contains(&serde_json::json!("role")));
    assert!(by_enum.contains(&serde_json::json!("label")));
    assert!(by_enum.contains(&serde_json::json!("placeholder")));
    assert!(
      schema
        .get("properties")
        .and_then(|properties| properties.get("name"))
        .is_some(),
      "role locator schema should expose name filter"
    );

    let expression = McpServer::build_locator_resolution_expression(&McpLocator {
      by: McpLocatorBy::Role,
      value: "button".to_string(),
      nth: Some(0),
      exact: Some(true),
      name: Some("登录".to_string()),
    })
    .expect("role locator expression should build");
    assert!(expression.contains("\"by\":\"role\""));
    assert!(expression.contains("\"name\":\"登录\""));
    assert!(expression.contains("getElementRole"));
    assert!(expression.contains("getElementAccessibleName"));
  }

  #[test]
  fn test_label_locator_expression_uses_label_resolution() {
    let expression = McpServer::build_locator_resolution_expression(&McpLocator {
      by: McpLocatorBy::Label,
      value: "Email".to_string(),
      nth: Some(0),
      exact: Some(false),
      name: None,
    })
    .expect("label locator expression should build");
    assert!(expression.contains("\"by\":\"label\""));
    assert!(expression.contains("getElementLabelText"));
    assert!(expression.contains("input, textarea, select, button"));
  }

  #[test]
  fn test_placeholder_locator_expression_uses_placeholder_resolution() {
    let expression = McpServer::build_locator_resolution_expression(&McpLocator {
      by: McpLocatorBy::Placeholder,
      value: "Search".to_string(),
      nth: Some(0),
      exact: Some(false),
      name: None,
    })
    .expect("placeholder locator expression should build");
    assert!(expression.contains("\"by\":\"placeholder\""));
    assert!(expression.contains("input[placeholder], textarea[placeholder]"));
    assert!(expression.contains("getAttribute(\"placeholder\")"));
  }

  #[test]
  fn test_single_target_locator_requires_nth_when_multiple_matches() {
    let locator = McpLocator {
      by: McpLocatorBy::Role,
      value: "button".to_string(),
      nth: None,
      exact: Some(false),
      name: Some("登录".to_string()),
    };
    let snapshot = LocatorResolutionSnapshot {
      locator: locator.clone(),
      strategy: McpLocatorBy::Role,
      index: 0,
      count: 2,
      exists: true,
      visible: true,
      text: "登录".to_string(),
      matched_texts: vec!["登录".to_string(), "登录并继续".to_string()],
      ambiguous: true,
      ambiguity_message: Some(
        "Locator matched 2 elements and no nth was provided. Pass locator.nth to disambiguate."
          .to_string(),
      ),
    };

    let error = McpServer::ensure_single_target_locator(&locator, &snapshot, "click_element")
      .expect_err("ambiguous locator should fail for single target operations");
    assert_eq!(error.code, MCP_ERROR_INVALID_PARAMS);
    assert!(error.message.contains("locator.nth"));

    let disambiguated_locator = McpLocator {
      nth: Some(1),
      ..locator
    };
    McpServer::ensure_single_target_locator(
      &disambiguated_locator,
      &LocatorResolutionSnapshot {
        locator: disambiguated_locator.clone(),
        ambiguous: false,
        ambiguity_message: None,
        ..snapshot
      },
      "click_element",
    )
    .expect("nth should disambiguate multi-match locator");
  }

  #[test]
  fn test_build_locator_collection_expression_serializes_limit_and_summary_shape() {
    let expression = McpServer::build_locator_collection_expression(
      &McpLocator {
        by: McpLocatorBy::Css,
        value: ".item".to_string(),
        nth: None,
        exact: None,
        name: None,
      },
      12,
    )
    .expect("locator collection expression should build");

    assert!(expression.contains("summarizeElement"));
    assert!(expression.contains("slice(0, limit)"));
    assert!(expression.contains("const limit = 12"));
  }

  #[test]
  fn test_read_tools_schema_expose_auto_wait_and_timeout_ms() {
    let server = McpServer::new();
    let tools = server.get_tools();

    for tool_name in ["element_exists", "get_element_text", "query_elements"] {
      let tool = tools
        .iter()
        .find(|tool| tool.name == tool_name)
        .expect("tool should exist");
      assert!(
        tool
          .input_schema
          .get("properties")
          .and_then(|properties| properties.get("auto_wait"))
          .is_some(),
        "{tool_name} should expose auto_wait"
      );
      assert!(
        tool
          .input_schema
          .get("properties")
          .and_then(|properties| properties.get("timeout_ms"))
          .is_some(),
        "{tool_name} should expose timeout_ms"
      );
    }
  }

  #[test]
  fn test_click_and_type_schema_expose_auto_wait() {
    let server = McpServer::new();
    let tools = server.get_tools();

    for tool_name in ["click_element", "type_text"] {
      let tool = tools
        .iter()
        .find(|tool| tool.name == tool_name)
        .expect("tool should exist");
      assert!(
        tool
          .input_schema
          .get("properties")
          .and_then(|properties| properties.get("auto_wait"))
          .is_some(),
        "{tool_name} should expose auto_wait"
      );
    }
  }

  #[test]
  fn test_build_element_target_expression_supports_selector_and_locator() {
    let selector_expression = McpServer::build_element_target_expression(
      Some("#login"),
      None,
      "return { ok: true, target };",
    )
    .expect("selector target expression should build");
    assert!(selector_expression.contains("\"#login\""));
    assert!(selector_expression.contains("querySelector"));

    let locator_expression = McpServer::build_element_target_expression(
      None,
      Some(&McpLocator {
        by: McpLocatorBy::Css,
        value: ".submit".to_string(),
        nth: Some(1),
        exact: None,
        name: None,
      }),
      "return { ok: true, target };",
    )
    .expect("locator target expression should build");
    assert!(locator_expression.contains("\"by\":\"css\""));
    assert!(locator_expression.contains("resolveLocatorSnapshot"));
    assert!(locator_expression.contains("resolution.ambiguous"));
  }

  #[test]
  fn test_build_element_actionability_expression_serializes_mode_and_checks() {
    let expression = McpServer::build_element_actionability_expression(
      None,
      Some(&McpLocator {
        by: McpLocatorBy::Text,
        value: "Submit".to_string(),
        nth: None,
        exact: Some(true),
        name: None,
      }),
      "click",
    )
    .expect("actionability expression should build");

    assert!(expression.contains("\"click\""));
    assert!(expression.contains("element_disabled"));
    assert!(expression.contains("focus_failed"));
    assert!(expression.contains("scrollIntoView"));
    assert!(expression.contains("resolution.ambiguous"));
  }

  #[test]
  fn test_build_wait_for_text_expression_serializes_scope_and_text() {
    let expression =
      McpServer::build_wait_for_text_expression("Welcome back", Some("#app"), None, true)
        .expect("wait text expression should build");

    assert!(expression.contains("\"Welcome back\""));
    assert!(expression.contains("\"#app\""));
    assert!(expression.contains("\"selector\""));
    assert!(expression.contains("scopeType"));
  }

  #[test]
  fn test_parse_locator_resolution_snapshot_accepts_unified_result_shape() {
    let snapshot = McpServer::parse_locator_resolution_snapshot(serde_json::json!({
      "locator": {
        "by": "css",
        "value": "#login",
        "nth": 0,
        "exact": false
      },
      "strategy": "css",
      "index": 0,
      "count": 1,
      "exists": true,
      "visible": true,
      "text": "Login",
      "matchedTexts": ["Login"],
      "ambiguous": false,
      "ambiguityMessage": null,
      "ok": true,
      "state": "visible"
    }))
    .expect("snapshot should parse");

    assert_eq!(snapshot.strategy, McpLocatorBy::Css);
    assert_eq!(snapshot.text, "Login");
    assert_eq!(snapshot.count, 1);
  }

  #[test]
  fn test_navigation_wait_satisfied_supports_url_change_and_reload() {
    let current = NavigationSnapshot {
      url: "https://example.com/dashboard".to_string(),
      title: "Dashboard".to_string(),
      ready_state: "complete".to_string(),
    };
    assert!(McpServer::navigation_wait_satisfied(
      "https://example.com/login",
      &current,
      false,
    ));

    let reloaded = NavigationSnapshot {
      url: "https://example.com/dashboard".to_string(),
      title: "Dashboard".to_string(),
      ready_state: "complete".to_string(),
    };
    assert!(McpServer::navigation_wait_satisfied(
      "https://example.com/dashboard",
      &reloaded,
      true,
    ));

    let idle = NavigationSnapshot {
      url: "https://example.com/dashboard".to_string(),
      title: "Dashboard".to_string(),
      ready_state: "complete".to_string(),
    };
    assert!(!McpServer::navigation_wait_satisfied(
      "https://example.com/dashboard",
      &idle,
      false,
    ));
  }

  #[tokio::test]
  async fn test_wait_for_selector_rejects_unsupported_state_before_profile_lookup() {
    let server = McpServer::new();
    let error = server
      .handle_wait_for_selector(&serde_json::json!({
        "profile_id": "missing-profile",
        "locator": {
          "by": "css",
          "value": "#app"
        },
        "state": "invalid"
      }))
      .await
      .expect_err("invalid state should fail");
    assert_eq!(error.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[tokio::test]
  async fn test_wait_for_selector_rejects_blank_locator_before_profile_lookup() {
    let server = McpServer::new();
    let error = server
      .handle_wait_for_selector(&serde_json::json!({
        "profile_id": "missing-profile",
        "locator": {
          "by": "css",
          "value": "   "
        }
      }))
      .await
      .expect_err("blank locator should fail");
    assert_eq!(error.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[tokio::test]
  async fn test_click_element_rejects_conflicting_target_before_profile_lookup() {
    let server = McpServer::new();
    let error = server
      .handle_click_element(&serde_json::json!({
        "profile_id": "missing-profile",
        "selector": "#submit",
        "locator": {
          "by": "css",
          "value": ".submit"
        }
      }))
      .await
      .expect_err("conflicting target should fail");
    assert_eq!(error.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[tokio::test]
  async fn test_click_element_rejects_missing_target_before_profile_lookup() {
    let server = McpServer::new();
    let error = server
      .handle_click_element(&serde_json::json!({
        "profile_id": "missing-profile"
      }))
      .await
      .expect_err("missing target should fail");
    assert_eq!(error.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[tokio::test]
  async fn test_type_text_rejects_blank_locator_before_profile_lookup() {
    let server = McpServer::new();
    let error = server
      .handle_type_text(&serde_json::json!({
        "profile_id": "missing-profile",
        "text": "hello",
        "locator": {
          "by": "css",
          "value": "   "
        }
      }))
      .await
      .expect_err("blank locator should fail");
    assert_eq!(error.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[tokio::test]
  async fn test_type_text_rejects_empty_text_before_profile_lookup() {
    let server = McpServer::new();
    let error = server
      .handle_type_text(&serde_json::json!({
        "profile_id": "missing-profile",
        "selector": "#search",
        "text": ""
      }))
      .await
      .expect_err("empty text should fail");
    assert_eq!(error.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[tokio::test]
  async fn test_element_exists_rejects_blank_locator_before_profile_lookup() {
    let server = McpServer::new();
    let error = server
      .handle_element_exists(&serde_json::json!({
        "profile_id": "missing-profile",
        "locator": {
          "by": "css",
          "value": "   "
        }
      }))
      .await
      .expect_err("blank locator should fail");
    assert_eq!(error.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[tokio::test]
  async fn test_get_element_text_rejects_blank_locator_before_profile_lookup() {
    let server = McpServer::new();
    let error = server
      .handle_get_element_text(&serde_json::json!({
        "profile_id": "missing-profile",
        "locator": {
          "by": "text",
          "value": "   "
        }
      }))
      .await
      .expect_err("blank locator should fail");
    assert_eq!(error.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[tokio::test]
  async fn test_query_elements_rejects_blank_locator_before_profile_lookup() {
    let server = McpServer::new();
    let error = server
      .handle_query_elements(&serde_json::json!({
        "profile_id": "missing-profile",
        "locator": {
          "by": "xpath",
          "value": "   "
        }
      }))
      .await
      .expect_err("blank locator should fail");
    assert_eq!(error.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[tokio::test]
  async fn test_count_elements_rejects_blank_locator_before_profile_lookup() {
    let server = McpServer::new();
    let error = server
      .handle_count_elements(&serde_json::json!({
        "profile_id": "missing-profile",
        "locator": {
          "by": "testid",
          "value": "   "
        }
      }))
      .await
      .expect_err("blank locator should fail");
    assert_eq!(error.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[tokio::test]
  async fn test_wait_for_text_rejects_conflicting_scope_before_profile_lookup() {
    let server = McpServer::new();
    let error = server
      .handle_wait_for_text(&serde_json::json!({
        "profile_id": "missing-profile",
        "text": "hello",
        "selector": "#app",
        "locator": {
          "by": "css",
          "value": ".card"
        }
      }))
      .await
      .expect_err("conflicting scope should fail");
    assert_eq!(error.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[tokio::test]
  async fn test_wait_for_text_rejects_blank_locator_before_profile_lookup() {
    let server = McpServer::new();
    let error = server
      .handle_wait_for_text(&serde_json::json!({
        "profile_id": "missing-profile",
        "text": "hello",
        "locator": {
          "by": "text",
          "value": "   "
        }
      }))
      .await
      .expect_err("blank locator should fail");
    assert_eq!(error.code, MCP_ERROR_INVALID_PARAMS);
  }

  #[tokio::test]
  async fn test_wait_for_navigation_rejects_empty_from_url_before_profile_lookup() {
    let server = McpServer::new();
    let error = server
      .handle_wait_for_navigation(&serde_json::json!({
        "profile_id": "missing-profile",
        "from_url": "   "
      }))
      .await
      .expect_err("empty from_url should fail");
    assert_eq!(error.code, MCP_ERROR_INVALID_PARAMS);
  }
}

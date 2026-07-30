//! Parallel batch coordinator — launch multiple independent registration
//! batches simultaneously, each with its own engine, CDK config, and VPN
//! peer pool. Staggers starts and caps total concurrency to avoid rate
//! limits and resource exhaustion.
//!
//! Designed for hands-free use by disabled users who cannot interact with
//! the browser.

use rand::RngExt;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::time::sleep;

use super::execution::{prepare_registration, run_prepared_registration};
use super::types::RegistrationConfig;

/// Maximum total parallel batches. Beyond this, new batches queue up.
const MAX_PARALLEL_BATCHES: usize = 6;

/// Minimum stagger delay between batch starts (milliseconds).
const MIN_STAGGER_MS: u64 = 2000;

/// Maximum stagger delay between batch starts (milliseconds).
const MAX_STAGGER_MS: u64 = 5000;

/// Result from a single parallel batch.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParallelBatchOutcome {
  /// The task ID assigned to this batch.
  pub task_id: String,
  /// Index in the original config array (0-based).
  pub batch_index: usize,
  /// Whether the batch completed without fatal error.
  pub success: bool,
  /// Number of accounts successfully registered in this batch.
  pub accounts_created: u32,
  /// Error message if the batch failed entirely.
  pub error: Option<String>,
}

/// Aggregated result from all parallel batches.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParallelBatchSummary {
  /// Total batches launched.
  pub total_batches: usize,
  /// Total accounts created across all batches.
  pub total_accounts: u32,
  /// Per-batch outcomes.
  pub outcomes: Vec<ParallelBatchOutcome>,
  /// Whether all batches succeeded.
  pub all_succeeded: bool,
}

fn prepare_all_batches_with<T, F>(
  configs: Vec<RegistrationConfig>,
  mut prepare: F,
) -> Result<Vec<T>, String>
where
  F: FnMut(RegistrationConfig) -> Result<T, String>,
{
  configs.into_iter().map(&mut prepare).collect()
}

/// Launch multiple registration batches in parallel.
///
/// Each batch runs its own `RegistrationEngine` on a dedicated thread.
/// Starts are staggered by a random 2–5 second delay to avoid triggering
/// rate limits from simultaneous first requests.
///
/// Returns a list of task IDs that can be used to track progress.
pub async fn launch_parallel_batches(
  app_handle: tauri::AppHandle,
  configs: Vec<RegistrationConfig>,
) -> Result<Vec<String>, String> {
  if configs.is_empty() {
    return Err("At least one registration config is required".into());
  }

  if configs.len() > MAX_PARALLEL_BATCHES {
    return Err(format!(
      "Too many parallel batches: {} > max {}",
      configs.len(),
      MAX_PARALLEL_BATCHES
    ));
  }

  let semaphore = Arc::new(Semaphore::new(MAX_PARALLEL_BATCHES));
  let cancel_flag = Arc::new(AtomicBool::new(false));
  let prepared_batches = prepare_all_batches_with(configs, |config| {
    prepare_registration(config, cancel_flag.clone())
  })?;
  let total_batches = prepared_batches.len();
  let mut task_ids = Vec::with_capacity(total_batches);

  // All configurations and selected-profile leases are prepared before the
  // first task starts, so a later invalid or busy profile cannot partially
  // launch the request.
  for (idx, prepared) in prepared_batches.into_iter().enumerate() {
    let handle = app_handle.clone();
    let flag = cancel_flag.clone();
    let task_id = prepared.task_id().to_string();
    task_ids.push(task_id.clone());

    let task_id_for_log = task_id.clone();

    // Acquire the semaphore permit on the current (Tauri command) runtime
    // before spawning the blocking thread. This gates the total number of
    // concurrently running batches.
    let permit = semaphore
      .clone()
      .acquire_owned()
      .await
      .map_err(|_| "parallel batch semaphore closed".to_string())?;

    super::task::spawn_registered(task_id.clone(), flag.clone(), move || {
      // Permit is moved into the closure; it auto-releases on drop
      // when the batch completes.
      let _permit = permit;

      log::info!(
        "Parallel batch {}/{} started: task={}",
        idx + 1,
        task_id_for_log,
        task_id_for_log
      );

      if let Err(error) = run_prepared_registration(handle, prepared) {
        log::error!("Parallel batch {task_id_for_log} failed: {error}");
      }
    })?;

    // Stagger starts — random delay between batches to avoid
    // simultaneous Cloudflare challenges.
    if idx + 1 < total_batches {
      let stagger_ms = rand::rng().random_range(MIN_STAGGER_MS..MAX_STAGGER_MS);
      std::thread::sleep(Duration::from_millis(stagger_ms));
    }
  }

  log::info!(
    "Launched {} parallel registration batches: {:?}",
    task_ids.len(),
    task_ids
  );

  Ok(task_ids)
}

/// Cancel all running parallel batches by their task IDs.
pub fn cancel_parallel_batches(task_ids: &[String]) -> u32 {
  let mut cancelled = 0u32;
  for task_id in task_ids {
    if super::task::cancel_task(task_id) {
      cancelled += 1;
    }
  }
  cancelled
}

/// Adaptive rate limiter — call before each network-sensitive operation
/// (navigate, submit, OTP request). If the operation returned a rate-limit
/// signal (HTTP 429, Cloudflare challenge, etc.), this increases the backoff.
#[derive(Debug, Clone)]
pub struct RateLimiter {
  /// Current backoff duration in milliseconds.
  backoff_ms: u64,
  /// Minimum backoff (never go below this).
  min_ms: u64,
  /// Maximum backoff (cap).
  max_ms: u64,
  /// Multiplier for each rate-limit event (geometric backoff).
  multiplier: f64,
  /// Consecutive rate-limit hits.
  consecutive_hits: u32,
  /// Consecutive successes (without rate limit).
  consecutive_ok: u32,
}

impl RateLimiter {
  /// Create a new rate limiter.
  ///
  /// Defaults: min 500ms, max 30s, multiplier 2.0x.
  pub fn new() -> Self {
    Self {
      backoff_ms: 0,
      min_ms: 500,
      max_ms: 30_000,
      multiplier: 2.0,
      consecutive_hits: 0,
      consecutive_ok: 0,
    }
  }

  /// Customize bounds.
  pub fn with_bounds(mut self, min_ms: u64, max_ms: u64, multiplier: f64) -> Self {
    self.min_ms = min_ms;
    self.max_ms = max_ms;
    self.multiplier = multiplier;
    self
  }

  /// Call this after each successful operation (no rate limit).
  /// Gradually reduces backoff when we've had enough clean runs.
  pub fn mark_success(&mut self) {
    self.consecutive_hits = 0;
    self.consecutive_ok += 1;
    // After 10 consecutive successes, halve the backoff
    if self.consecutive_ok >= 10 && self.backoff_ms > self.min_ms {
      self.backoff_ms = (self.backoff_ms / 2).max(self.min_ms);
      self.consecutive_ok = 0;
    }
  }

  /// Call this after a rate-limit event (429, Cloudflare, auth challenge).
  /// Increases backoff geometrically.
  pub fn mark_rate_limited(&mut self) {
    self.consecutive_ok = 0;
    self.consecutive_hits += 1;
    if self.backoff_ms == 0 {
      self.backoff_ms = self.min_ms;
    } else {
      let new = (self.backoff_ms as f64 * self.multiplier) as u64;
      self.backoff_ms = new.clamp(self.min_ms, self.max_ms);
    }
    log::warn!(
      "Rate limiter: backoff increased to {}ms (hit #{})",
      self.backoff_ms,
      self.consecutive_hits
    );
  }

  /// Wait for the current backoff duration. Returns immediately if
  /// there's no active backoff.
  pub async fn wait_if_needed(&self) {
    if self.backoff_ms > 0 {
      log::debug!(
        "Rate limiter: sleeping {}ms before next request",
        self.backoff_ms
      );
      sleep(Duration::from_millis(self.backoff_ms)).await;
    }
  }

  /// Get current backoff duration.
  pub fn current_backoff_ms(&self) -> u64 {
    self.backoff_ms
  }

  /// Whether the limiter is currently throttling.
  pub fn is_throttling(&self) -> bool {
    self.backoff_ms > 0 && self.consecutive_hits > 0
  }
}

impl Default for RateLimiter {
  fn default() -> Self {
    Self::new()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parallel_preparation_returns_no_batches_when_a_later_config_fails() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct PreparedMarker(Arc<AtomicUsize>);

    impl Drop for PreparedMarker {
      fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
      }
    }

    let configs = vec![
      serde_json::from_value(serde_json::json!({ "cdks": ["FIRST"] })).unwrap(),
      serde_json::from_value(serde_json::json!({ "cdks": ["SECOND"] })).unwrap(),
    ];
    let dropped = Arc::new(AtomicUsize::new(0));
    let mut prepared = 0usize;

    let result = prepare_all_batches_with(configs, |_config| {
      prepared += 1;
      if prepared == 2 {
        Err("second preparation failed".to_string())
      } else {
        Ok(PreparedMarker(dropped.clone()))
      }
    });

    assert_eq!(result.unwrap_err(), "second preparation failed");
    assert_eq!(prepared, 2);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
  }

  #[test]
  fn rate_limiter_starts_idle() {
    let rl = RateLimiter::new();
    assert_eq!(rl.current_backoff_ms(), 0);
    assert!(!rl.is_throttling());
  }

  #[test]
  fn rate_limiter_backs_off_on_hit() {
    let mut rl = RateLimiter::new();
    rl.mark_rate_limited();
    assert!(rl.current_backoff_ms() >= 500);
    assert!(rl.is_throttling());
  }

  #[test]
  fn rate_limiter_grows_exponentially() {
    let mut rl = RateLimiter::new();
    rl.mark_rate_limited();
    let first = rl.current_backoff_ms();
    rl.mark_rate_limited();
    let second = rl.current_backoff_ms();
    assert!(second > first, "backoff should grow: {first} → {second}");
  }

  #[test]
  fn rate_limiter_recovers_on_success() {
    let mut rl = RateLimiter::new();
    rl.mark_rate_limited();
    assert!(rl.current_backoff_ms() > 0);
    // 10 successes should halve it
    for _ in 0..10 {
      rl.mark_success();
    }
    // Should be at least halved
    assert!(rl.current_backoff_ms() < 1000);
  }

  #[test]
  fn rate_limiter_respects_max() {
    let mut rl = RateLimiter::new().with_bounds(100, 2000, 10.0);
    for _ in 0..10 {
      rl.mark_rate_limited();
    }
    assert!(rl.current_backoff_ms() <= 2000);
  }
}

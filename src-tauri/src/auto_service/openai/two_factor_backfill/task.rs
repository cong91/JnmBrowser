use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;

/// A running 2FA backfill task. The task itself owns its cleanup guard; the
/// registry only needs the cancellation handle.
pub(crate) struct BackfillTaskHandle {
  pub cancel_flag: Arc<AtomicBool>,
}

/// Registry of running 2FA backfill tasks. Completed handles must be removed
/// by the engine on every terminal path so the registry never retains stale
/// state for an operation that already persisted its final journal/account state.
static TASKS: Lazy<Mutex<HashMap<String, BackfillTaskHandle>>> =
  Lazy::new(|| Mutex::new(HashMap::new()));

pub(crate) fn register_task(task_id: String, handle: BackfillTaskHandle) {
  TASKS.lock().unwrap().insert(task_id, handle);
}

struct TaskCleanup {
  task_id: String,
}

impl Drop for TaskCleanup {
  fn drop(&mut self) {
    remove_task(&self.task_id);
  }
}

pub(crate) fn spawn_registered<F>(
  task_id: String,
  cancel_flag: Arc<AtomicBool>,
  future: F,
) -> Result<(), String>
where
  F: Future<Output = ()> + Send + 'static,
{
  let (start_tx, start_rx) = tokio::sync::oneshot::channel();
  let task_id_for_cleanup = task_id.clone();
  tokio::spawn(async move {
    let _cleanup = TaskCleanup {
      task_id: task_id_for_cleanup,
    };
    if start_rx.await.is_ok() {
      future.await;
    }
  });
  register_task(task_id, BackfillTaskHandle { cancel_flag });
  start_tx
    .send(())
    .map_err(|_| "2FA backfill task ended before startup was released".to_string())
}

pub(crate) fn cancel_task(task_id: &str) -> bool {
  let tasks = TASKS.lock().unwrap();
  if let Some(handle) = tasks.get(task_id) {
    handle.cancel_flag.store(true, Ordering::SeqCst);
    true
  } else {
    false
  }
}

pub(crate) fn remove_task(task_id: &str) {
  TASKS.lock().unwrap().remove(task_id);
}

#[cfg(test)]
mod tests {
  use super::*;

  fn drain(task_id: &str) {
    TASKS.lock().unwrap().remove(task_id);
  }

  #[tokio::test]
  async fn register_cancel_and_remove_round_trip() {
    let task_id = format!("backfill-task-{}", uuid::Uuid::new_v4());
    let cancel_flag = Arc::new(AtomicBool::new(false));
    register_task(
      task_id.clone(),
      BackfillTaskHandle {
        cancel_flag: cancel_flag.clone(),
      },
    );
    assert!(cancel_task(&task_id));
    assert!(cancel_flag.load(Ordering::SeqCst));
    remove_task(&task_id);
    assert!(!cancel_task(&task_id));
    drain(&task_id);
  }

  #[tokio::test]
  async fn spawn_registered_publishes_before_running_and_cleans_after_completion() {
    let task_id = format!("backfill-task-{}", uuid::Uuid::new_v4());
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let task_id_for_future = task_id.clone();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();

    spawn_registered(task_id.clone(), cancel_flag, async move {
      started_tx
        .send(cancel_task(&task_id_for_future))
        .expect("test receiver must remain alive");
    })
    .expect("registered task should start");

    assert!(
      started_rx.await.expect("task must report registry state"),
      "task started before its cancellation handle was registered"
    );
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
      while cancel_task(&task_id) {
        tokio::task::yield_now().await;
      }
    })
    .await
    .expect("completed task must be removed from the registry");
  }

  #[tokio::test]
  async fn cancel_unknown_task_returns_false() {
    let missing = format!("missing-{}", uuid::Uuid::new_v4());
    assert!(!cancel_task(&missing));
  }
}

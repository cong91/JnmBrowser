use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

use once_cell::sync::Lazy;

/// A running login task. The blocking task owns its cleanup guard; the registry
/// retains only the cancellation handle.
pub struct LoginTaskHandle {
  pub cancel_flag: Arc<AtomicBool>,
}

static TASKS: Lazy<Mutex<HashMap<String, LoginTaskHandle>>> =
  Lazy::new(|| Mutex::new(HashMap::new()));

fn register_task(task_id: String, handle: LoginTaskHandle) {
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

pub fn spawn_registered<F>(
  task_id: String,
  cancel_flag: Arc<AtomicBool>,
  work: F,
) -> Result<(), String>
where
  F: FnOnce() + Send + 'static,
{
  let (start_tx, start_rx) = std::sync::mpsc::channel();
  let task_id_for_cleanup = task_id.clone();
  tokio::task::spawn_blocking(move || {
    let _cleanup = TaskCleanup {
      task_id: task_id_for_cleanup,
    };
    if start_rx.recv().is_ok() {
      work();
    }
  });
  register_task(task_id.clone(), LoginTaskHandle { cancel_flag });
  start_tx.send(()).map_err(|_| {
    remove_task(&task_id);
    "Login task ended before startup was released".to_string()
  })
}

pub fn cancel_task(task_id: &str) -> bool {
  if let Some(handle) = TASKS.lock().unwrap().get(task_id) {
    handle.cancel_flag.store(true, Ordering::SeqCst);
    true
  } else {
    false
  }
}

fn remove_task(task_id: &str) {
  TASKS.lock().unwrap().remove(task_id);
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn spawn_registered_publishes_before_running_and_cleans_after_completion() {
    let task_id = format!("login-task-{}", uuid::Uuid::new_v4());
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let task_id_for_work = task_id.clone();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();

    spawn_registered(task_id.clone(), cancel_flag, move || {
      started_tx
        .send(cancel_task(&task_id_for_work))
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

  #[test]
  fn cancel_unknown_task_returns_false() {
    let missing = format!("missing-{}", uuid::Uuid::new_v4());
    assert!(!cancel_task(&missing));
  }
}

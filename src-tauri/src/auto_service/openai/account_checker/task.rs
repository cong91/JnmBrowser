// TODO: remove allow when browser integration is complete.
#![allow(dead_code)]

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;

pub(crate) struct AccountCheckTaskHandle {
  pub cancel_flag: Arc<AtomicBool>,
}

static TASKS: Lazy<Mutex<HashMap<String, AccountCheckTaskHandle>>> =
  Lazy::new(|| Mutex::new(HashMap::new()));

pub(crate) fn register_task(task_id: String, handle: AccountCheckTaskHandle) {
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
  register_task(task_id, AccountCheckTaskHandle { cancel_flag });
  start_tx
    .send(())
    .map_err(|_| "Account check task ended before startup was released".to_string())
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

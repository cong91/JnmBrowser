use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

use once_cell::sync::Lazy;

pub struct TaskHandle {
  pub cancel_flag: Arc<AtomicBool>,
  pub join_handle: tokio::task::JoinHandle<Vec<super::types::LoginResult>>,
}

static TASKS: Lazy<Mutex<HashMap<String, TaskHandle>>> = Lazy::new(|| Mutex::new(HashMap::new()));

pub fn register_task(task_id: String, handle: TaskHandle) {
  TASKS.lock().unwrap().insert(task_id, handle);
}

pub fn cancel_task(task_id: &str) -> bool {
  let tasks = TASKS.lock().unwrap();
  if let Some(handle) = tasks.get(task_id) {
    handle.cancel_flag.store(true, Ordering::SeqCst);
    true
  } else {
    false
  }
}

#[allow(dead_code)]
pub fn remove_task(task_id: &str) {
  TASKS.lock().unwrap().remove(task_id);
}

#[allow(dead_code)]
pub fn is_cancelled(task_id: &str) -> bool {
  let tasks = TASKS.lock().unwrap();
  tasks
    .get(task_id)
    .map(|h| h.cancel_flag.load(Ordering::SeqCst))
    .unwrap_or(true)
}

use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use tokio::task::JoinHandle;

use super::types::RegistrationResult;

/// Handle to a running registration task.
pub struct TaskHandle {
  pub cancel_flag: Arc<AtomicBool>,
  pub join_handle: JoinHandle<RegistrationResult>,
}

/// Global registry of running registration tasks.
static TASKS: Lazy<Mutex<HashMap<String, TaskHandle>>> = Lazy::new(|| Mutex::new(HashMap::new()));

pub fn register_task(task_id: String, handle: TaskHandle) {
  TASKS.lock().unwrap().insert(task_id, handle);
}

pub fn cancel_task(task_id: &str) -> bool {
  if let Some(handle) = TASKS.lock().unwrap().get(task_id) {
    handle
      .cancel_flag
      .store(true, std::sync::atomic::Ordering::SeqCst);
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
pub fn cleanup_completed() {
  TASKS
    .lock()
    .unwrap()
    .retain(|_id, handle| !handle.join_handle.is_finished());
}

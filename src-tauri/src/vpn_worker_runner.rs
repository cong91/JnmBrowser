use crate::proxy_runner::find_sidecar_executable;
use crate::proxy_storage::is_process_running;
use crate::vpn_worker_storage::{
  delete_vpn_worker_config, find_vpn_worker_by_vpn_id, generate_vpn_worker_id,
  get_vpn_worker_config, list_vpn_worker_configs, save_vpn_worker_config_to_path,
  vpn_worker_config_path, VpnWorkerConfig,
};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};

const VPN_WORKER_POLL_INTERVAL_MS: u64 = 100;
const VPN_WORKER_STARTUP_TIMEOUT_MS: u64 = 30_000;

trait StartupProcess: Send {
  fn id(&self) -> u32;
  fn terminate(&mut self) -> std::io::Result<()>;
}

impl StartupProcess for Child {
  fn id(&self) -> u32 {
    Child::id(self)
  }

  fn terminate(&mut self) -> std::io::Result<()> {
    if self.try_wait()?.is_some() {
      return Ok(());
    }

    match self.kill() {
      Ok(()) => {
        let _ = self.wait()?;
        Ok(())
      }
      Err(error) => {
        if self.try_wait()?.is_some() {
          Ok(())
        } else {
          Err(error)
        }
      }
    }
  }
}

struct VpnWorkerStartupGuard {
  process: Option<Box<dyn StartupProcess>>,
  config_file_path: PathBuf,
  mapping_path: PathBuf,
  armed: bool,
}

impl VpnWorkerStartupGuard {
  fn new(config_file_path: PathBuf, mapping_path: PathBuf) -> Self {
    Self {
      process: None,
      config_file_path,
      mapping_path,
      armed: true,
    }
  }

  fn attach_process(&mut self, process: impl StartupProcess + 'static) {
    debug_assert!(self.process.is_none());
    self.process = Some(Box::new(process));
  }

  fn process_id(&self) -> Option<u32> {
    self.process.as_ref().map(|process| process.id())
  }

  fn disarm(&mut self) {
    self.armed = false;
  }
}

impl Drop for VpnWorkerStartupGuard {
  fn drop(&mut self) {
    if !self.armed {
      return;
    }

    if let Some(process) = self.process.as_mut() {
      if let Err(error) = process.terminate() {
        log::warn!(
          "Failed to terminate VPN worker process {} during startup rollback: {error}",
          process.id()
        );
      }
    }

    cleanup_vpn_worker_artifacts(&self.config_file_path, &self.mapping_path);
  }
}

fn cleanup_vpn_worker_artifacts(config_file_path: &Path, mapping_path: &Path) {
  for (kind, path) in [
    ("temporary config", config_file_path),
    ("worker mapping", mapping_path),
  ] {
    if let Err(error) = std::fs::remove_file(path) {
      if error.kind() != std::io::ErrorKind::NotFound {
        log::warn!(
          "Failed to remove VPN worker {kind} {}: {error}",
          path.display()
        );
      }
    }
  }
}

async fn vpn_worker_accepting_connections(config: &VpnWorkerConfig) -> bool {
  let Some(port) = config.local_port else {
    return false;
  };

  if config
    .local_url
    .as_ref()
    .is_none_or(|local_url| local_url.is_empty())
  {
    return false;
  }

  matches!(
    tokio::time::timeout(
      tokio::time::Duration::from_millis(VPN_WORKER_POLL_INTERVAL_MS),
      tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await,
    Ok(Ok(_))
  )
}

fn worker_log_path(id: &str) -> std::path::PathBuf {
  std::env::temp_dir().join(format!("donut-vpn-{}.log", id))
}

fn read_worker_log(id: &str) -> String {
  std::fs::read_to_string(worker_log_path(id)).unwrap_or_else(|_| "No log available".to_string())
}

async fn wait_for_vpn_worker_ready(
  id: &str,
) -> Result<VpnWorkerConfig, Box<dyn std::error::Error>> {
  let startup_timeout = tokio::time::Duration::from_millis(VPN_WORKER_STARTUP_TIMEOUT_MS);
  let startup_deadline = tokio::time::Instant::now() + startup_timeout;

  tokio::time::sleep(tokio::time::Duration::from_millis(
    VPN_WORKER_POLL_INTERVAL_MS,
  ))
  .await;

  let mut attempts = 0u32;

  loop {
    tokio::time::sleep(tokio::time::Duration::from_millis(
      VPN_WORKER_POLL_INTERVAL_MS,
    ))
    .await;

    if let Some(updated_config) = get_vpn_worker_config(id) {
      let process_running = updated_config.pid.map(is_process_running).unwrap_or(false);

      if !process_running && attempts > 2 {
        let log_output = read_worker_log(id);
        return Err(format!("VPN worker process crashed. Log output:\n{}", log_output).into());
      }

      if vpn_worker_accepting_connections(&updated_config).await {
        return Ok(updated_config);
      }
    }

    attempts += 1;
    if tokio::time::Instant::now() >= startup_deadline {
      if let Some(config) = get_vpn_worker_config(id) {
        let process_running = config.pid.map(is_process_running).unwrap_or(false);
        let log_output = read_worker_log(id);
        return Err(
          format!(
            "VPN worker failed to start within {:.1}s. pid={:?}, process_running={}, local_url={:?}\n\nVPN worker log:\n{}",
            startup_timeout.as_secs_f32(),
            config.pid,
            process_running,
            config.local_url,
            log_output
          )
          .into(),
        );
      }

      return Err("VPN worker config not found after spawn".into());
    }
  }
}

pub async fn start_vpn_worker(vpn_id: &str) -> Result<VpnWorkerConfig, Box<dyn std::error::Error>> {
  for config in list_vpn_worker_configs() {
    if let Some(pid) = config.pid {
      if !is_process_running(pid) {
        delete_vpn_worker_config(&config.id);
      }
    } else {
      delete_vpn_worker_config(&config.id);
    }
  }

  // Check if a VPN worker for this vpn_id already exists and is running
  if let Some(existing) = find_vpn_worker_by_vpn_id(vpn_id) {
    if let Some(pid) = existing.pid {
      if is_process_running(pid) {
        if vpn_worker_accepting_connections(&existing).await {
          return Ok(existing);
        }

        return wait_for_vpn_worker_ready(&existing.id).await;
      }
    }
    // Worker config exists but process is dead, clean up
    delete_vpn_worker_config(&existing.id);
  }

  // Load VPN config from storage to determine type
  let vpn_config = {
    let storage = crate::vpn::VPN_STORAGE
      .lock()
      .map_err(|e| format!("Failed to lock VPN storage: {e}"))?;
    storage
      .load_config(vpn_id)
      .map_err(|e| format!("Failed to load VPN config: {e}"))?
  };

  let vpn_type_str = "wireguard";

  let id = generate_vpn_worker_id();
  let config_file_path = std::env::temp_dir()
    .join(format!("donut_vpn_{id}.conf"))
    .to_string_lossy()
    .to_string();
  let config_json_path = vpn_worker_config_path(&id);
  let mut startup_guard =
    VpnWorkerStartupGuard::new(PathBuf::from(&config_file_path), config_json_path.clone());

  // Write decrypted config to a worker-unique temp file.
  std::fs::write(&config_file_path, &vpn_config.config_data)?;

  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&config_file_path, std::fs::Permissions::from_mode(0o600))?;
  }

  // Find an available port
  let local_port = {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    listener.local_addr()?.port()
  };

  let config = VpnWorkerConfig::new(
    id.clone(),
    vpn_id.to_string(),
    vpn_type_str.to_string(),
    config_file_path,
  );
  save_vpn_worker_config_to_path(&config, &config_json_path)?;

  // Spawn detached VPN worker process
  let exe = find_sidecar_executable("donut-proxy")?;

  #[cfg(unix)]
  {
    use std::os::unix::process::CommandExt;
    use std::process::Command as StdCommand;

    let mut cmd = StdCommand::new(&exe);
    cmd.arg("vpn-worker");
    cmd.arg("start");
    cmd.arg("--id");
    cmd.arg(&id);
    cmd.arg("--port");
    cmd.arg(local_port.to_string());
    cmd.arg("--config-path");
    cmd.arg(&config_json_path);

    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());

    let log_path = std::env::temp_dir().join(format!("donut-vpn-{}.log", id));
    if let Ok(file) = std::fs::File::create(&log_path) {
      log::info!("VPN worker stderr will be logged to: {:?}", log_path);
      cmd.stderr(Stdio::from(file));
    } else {
      cmd.stderr(Stdio::null());
    }

    unsafe {
      cmd.pre_exec(|| {
        libc::setsid();
        if libc::setpriority(libc::PRIO_PROCESS, 0, -10) != 0 {
          let _ = libc::setpriority(libc::PRIO_PROCESS, 0, -5);
        }
        Ok(())
      });
    }

    let child = cmd.spawn()?;
    startup_guard.attach_process(child);
    let pid = startup_guard
      .process_id()
      .expect("spawned VPN worker guard must own a process");

    let mut config_with_pid = config.clone();
    config_with_pid.pid = Some(pid);
    config_with_pid.local_port = Some(local_port);
    save_vpn_worker_config_to_path(&config_with_pid, &config_json_path)?;
  }

  #[cfg(windows)]
  {
    use std::os::windows::process::CommandExt;
    use std::process::Command as StdCommand;

    let mut cmd = StdCommand::new(&exe);
    cmd.arg("vpn-worker");
    cmd.arg("start");
    cmd.arg("--id");
    cmd.arg(&id);
    cmd.arg("--port");
    cmd.arg(local_port.to_string());
    cmd.arg("--config-path");
    cmd.arg(&config_json_path);

    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());

    let log_path = std::env::temp_dir().join(format!("donut-vpn-{}.log", id));
    if let Ok(file) = std::fs::File::create(&log_path) {
      log::info!("VPN worker stderr will be logged to: {:?}", log_path);
      cmd.stderr(Stdio::from(file));
    } else {
      cmd.stderr(Stdio::null());
    }

    const DETACHED_PROCESS: u32 = 0x00000008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);

    let child = cmd.spawn()?;
    startup_guard.attach_process(child);
    let pid = startup_guard
      .process_id()
      .expect("spawned VPN worker guard must own a process");

    let mut config_with_pid = config.clone();
    config_with_pid.pid = Some(pid);
    config_with_pid.local_port = Some(local_port);
    save_vpn_worker_config_to_path(&config_with_pid, &config_json_path)?;
  }

  let ready_config = wait_for_vpn_worker_ready(&id).await?;
  startup_guard.disarm();
  Ok(ready_config)
}

pub async fn stop_vpn_worker(id: &str) -> Result<bool, Box<dyn std::error::Error>> {
  let config = get_vpn_worker_config(id);

  if let Some(config) = config {
    if let Some(pid) = config.pid {
      #[cfg(unix)]
      {
        use std::process::Command;
        let _ = Command::new("kill")
          .arg("-TERM")
          .arg(pid.to_string())
          .output();
      }
      #[cfg(windows)]
      {
        use std::os::windows::process::CommandExt;
        use std::process::Command;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        let _ = Command::new("taskkill")
          .args(["/F", "/PID", &pid.to_string()])
          .creation_flags(CREATE_NO_WINDOW)
          .output();
      }

      tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    cleanup_vpn_worker_artifacts(
      Path::new(&config.config_file_path),
      &vpn_worker_config_path(id),
    );
    return Ok(true);
  }

  Ok(false)
}

pub async fn stop_vpn_worker_by_vpn_id(vpn_id: &str) -> Result<bool, Box<dyn std::error::Error>> {
  if let Some(config) = find_vpn_worker_by_vpn_id(vpn_id) {
    return stop_vpn_worker(&config.id).await;
  }
  Ok(false)
}

pub async fn stop_all_vpn_workers() -> Result<(), Box<dyn std::error::Error>> {
  let configs = list_vpn_worker_configs();
  for config in configs {
    let _ = stop_vpn_worker(&config.id).await;
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::{Path, PathBuf};
  use std::sync::{Arc, Mutex};

  struct FakeProcess {
    id: u32,
    terminated_processes: Arc<Mutex<Vec<u32>>>,
  }

  impl StartupProcess for FakeProcess {
    fn id(&self) -> u32 {
      self.id
    }

    fn terminate(&mut self) -> std::io::Result<()> {
      self.terminated_processes.lock().unwrap().push(self.id);
      Ok(())
    }
  }

  fn create_worker_artifacts(root: &Path, worker_id: &str, vpn_id: &str) -> (PathBuf, PathBuf) {
    let config_path = root.join(format!("{worker_id}.conf"));
    std::fs::write(&config_path, format!("config for {vpn_id}")).unwrap();

    let mapping_path = root.join(format!("vpn_worker_{worker_id}.json"));
    let config = VpnWorkerConfig::new(
      worker_id.to_string(),
      vpn_id.to_string(),
      "wireguard".to_string(),
      config_path.to_string_lossy().into_owned(),
    );
    crate::vpn_worker_storage::save_vpn_worker_config_to_path(&config, &mapping_path).unwrap();

    (config_path, mapping_path)
  }

  #[test]
  fn startup_error_after_spawn_terminates_exact_child_and_removes_owned_config() {
    let temp_dir = tempfile::tempdir().unwrap();
    let config_path = temp_dir.path().join("spawned.conf");
    let mapping_path = temp_dir.path().join("not-yet-created.json");
    std::fs::write(&config_path, "secret").unwrap();
    let terminated_processes = Arc::new(Mutex::new(Vec::new()));

    let result: Result<(), &'static str> = {
      let mut guard = VpnWorkerStartupGuard::new(config_path.clone(), mapping_path.clone());
      guard.attach_process(FakeProcess {
        id: 41,
        terminated_processes: Arc::clone(&terminated_processes),
      });
      Err("failure after spawn")
    };

    assert_eq!(result, Err("failure after spawn"));
    assert_eq!(*terminated_processes.lock().unwrap(), vec![41]);
    assert!(!config_path.exists());
    assert!(!mapping_path.exists());
  }

  #[test]
  fn startup_error_after_mapping_removes_only_owned_same_vpn_worker() {
    let temp_dir = tempfile::tempdir().unwrap();
    let (owned_config, owned_mapping) =
      create_worker_artifacts(temp_dir.path(), "owned-worker", "shared-vpn");
    let (peer_config, peer_mapping) =
      create_worker_artifacts(temp_dir.path(), "peer-worker", "shared-vpn");
    let terminated_processes = Arc::new(Mutex::new(Vec::new()));

    let result: Result<(), &'static str> = {
      let mut guard = VpnWorkerStartupGuard::new(owned_config.clone(), owned_mapping.clone());
      guard.attach_process(FakeProcess {
        id: 42,
        terminated_processes: Arc::clone(&terminated_processes),
      });
      Err("failure after mapping")
    };

    assert_eq!(result, Err("failure after mapping"));
    assert_eq!(*terminated_processes.lock().unwrap(), vec![42]);
    assert!(!owned_config.exists());
    assert!(!owned_mapping.exists());
    assert!(peer_config.exists());
    assert!(peer_mapping.exists());
  }

  #[test]
  fn startup_panic_terminates_exact_child_and_removes_owned_artifacts() {
    let temp_dir = tempfile::tempdir().unwrap();
    let (config_path, mapping_path) =
      create_worker_artifacts(temp_dir.path(), "panicked-worker", "shared-vpn");
    let terminated_processes = Arc::new(Mutex::new(Vec::new()));

    let panic_result = std::panic::catch_unwind({
      let terminated_processes = Arc::clone(&terminated_processes);
      let config_path = config_path.clone();
      let mapping_path = mapping_path.clone();
      move || {
        let mut guard = VpnWorkerStartupGuard::new(config_path, mapping_path);
        guard.attach_process(FakeProcess {
          id: 44,
          terminated_processes,
        });
        panic!("failure during startup");
      }
    });

    assert!(panic_result.is_err());
    assert_eq!(*terminated_processes.lock().unwrap(), vec![44]);
    assert!(!config_path.exists());
    assert!(!mapping_path.exists());
  }

  #[test]
  fn successful_startup_disarms_guard_and_normal_cleanup_removes_artifacts() {
    let temp_dir = tempfile::tempdir().unwrap();
    let (config_path, mapping_path) =
      create_worker_artifacts(temp_dir.path(), "successful-worker", "shared-vpn");
    let terminated_processes = Arc::new(Mutex::new(Vec::new()));

    let mut guard = VpnWorkerStartupGuard::new(config_path.clone(), mapping_path.clone());
    guard.attach_process(FakeProcess {
      id: 43,
      terminated_processes: Arc::clone(&terminated_processes),
    });
    guard.disarm();

    assert!(terminated_processes.lock().unwrap().is_empty());
    assert!(config_path.exists());
    assert!(mapping_path.exists());

    cleanup_vpn_worker_artifacts(&config_path, &mapping_path);

    assert!(terminated_processes.lock().unwrap().is_empty());
    assert!(!config_path.exists());
    assert!(!mapping_path.exists());
  }
}

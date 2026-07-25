use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::profile::BrowserProfile;

lazy_static::lazy_static! {
  static ref EPHEMERAL_DIRS: Mutex<HashMap<String, PathBuf>> = Mutex::new(HashMap::new());
  static ref ORPHANED_EPHEMERAL_BACKUPS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
}

/// Get or create the RAM-backed base directory for ephemeral profiles.
/// Linux: /dev/shm (always tmpfs). macOS: RAM disk via hdiutil. Windows: imdisk RAM disk.
fn get_ephemeral_base_dir() -> Result<PathBuf, String> {
  #[cfg(target_os = "linux")]
  {
    let base = PathBuf::from("/dev/shm/donut-ephemeral");
    std::fs::create_dir_all(&base)
      .map_err(|e| format!("Failed to create ephemeral base in /dev/shm: {e}"))?;
    Ok(base)
  }

  #[cfg(not(target_os = "linux"))]
  {
    #[cfg(target_os = "macos")]
    {
      if let Ok(mount) = get_or_create_macos_ramdisk() {
        return Ok(mount);
      }
      log::warn!("Failed to create macOS RAM disk, ephemeral profiles may use disk");
    }

    #[cfg(target_os = "windows")]
    {
      if let Ok(mount) = get_or_create_windows_ramdisk() {
        return Ok(mount);
      }
      // imdisk is optional. Falling back to %TEMP% is expected on most machines.
      log::debug!(
        "Windows RAM disk unavailable (imdisk not installed/usable); ephemeral profiles use temp disk"
      );
    }

    // Fallback
    let base = std::env::temp_dir().join("donut-ephemeral");
    std::fs::create_dir_all(&base)
      .map_err(|e| format!("Failed to create ephemeral base dir: {e}"))?;
    Ok(base)
  }
}

#[cfg(target_os = "macos")]
fn get_or_create_macos_ramdisk() -> Result<PathBuf, String> {
  let mount_point = PathBuf::from("/Volumes/DonutEphemeral");

  // Reuse existing RAM disk from a previous session
  if mount_point.exists() && mount_point.is_dir() {
    return Ok(mount_point);
  }

  // 256 MB in 512-byte sectors
  let sectors = 256 * 2048;
  let output = std::process::Command::new("hdiutil")
    .args(["attach", "-nomount", &format!("ram://{sectors}")])
    .output()
    .map_err(|e| format!("hdiutil attach failed: {e}"))?;

  if !output.status.success() {
    return Err(format!(
      "hdiutil attach failed: {}",
      String::from_utf8_lossy(&output.stderr)
    ));
  }

  let dev = String::from_utf8_lossy(&output.stdout).trim().to_string();

  let fmt = std::process::Command::new("diskutil")
    .args(["erasevolume", "HFS+", "DonutEphemeral", &dev])
    .output()
    .map_err(|e| format!("diskutil erasevolume failed: {e}"))?;

  if !fmt.status.success() {
    let _ = std::process::Command::new("hdiutil")
      .args(["detach", &dev])
      .output();
    return Err(format!(
      "diskutil erasevolume failed: {}",
      String::from_utf8_lossy(&fmt.stderr)
    ));
  }

  log::info!("Created macOS RAM disk at {}", mount_point.display());
  Ok(mount_point)
}

#[cfg(target_os = "windows")]
fn get_or_create_windows_ramdisk() -> Result<PathBuf, String> {
  // Check if a previous RAM disk with our directory already exists
  for letter in ['R', 'Q', 'P', 'O'] {
    let base = PathBuf::from(format!("{}:\\DonutEphemeral", letter));
    if base.exists() && base.is_dir() {
      return Ok(base);
    }
  }

  // Try to create a RAM disk using imdisk (open-source RAM disk driver)
  for letter in ['R', 'Q', 'P', 'O'] {
    let drive = format!("{}:", letter);
    if PathBuf::from(format!("{}\\", drive)).exists() {
      continue;
    }

    let output = std::process::Command::new("imdisk")
      .args(["-a", "-s", "256M", "-m", &drive, "-p", "/fs:ntfs /q /y"])
      .output();

    match output {
      Ok(out) if out.status.success() => {
        let base = PathBuf::from(format!("{}\\DonutEphemeral", drive));
        std::fs::create_dir_all(&base)
          .map_err(|e| format!("Failed to create dir on RAM disk: {e}"))?;
        log::info!("Created Windows RAM disk at {}", base.display());
        return Ok(base);
      }
      Ok(out) => {
        log::debug!(
          "imdisk failed for drive {}: {}",
          drive,
          String::from_utf8_lossy(&out.stderr)
        );
      }
      Err(e) => {
        return Err(format!("imdisk not available: {e}"));
      }
    }
  }

  Err("Could not create Windows RAM disk".to_string())
}

pub fn create_ephemeral_dir(profile_id: &str) -> Result<PathBuf, String> {
  cleanup_orphaned_ephemeral_backups();

  let base = get_ephemeral_base_dir()?;
  let dir_path = base.join(profile_id);
  let mut dirs = EPHEMERAL_DIRS
    .lock()
    .map_err(|e| format!("Failed to lock ephemeral dirs: {e}"))?;
  let previous_mapping = dirs.get(profile_id).cloned();
  let mut backup_path = None;

  if dir_path.exists() {
    let backup = unique_backup_path(&base, profile_id);
    rename_ephemeral_dir(&dir_path, &backup).map_err(|e| {
      format!(
        "Failed to preserve existing ephemeral dir {} before replacement: {e}",
        dir_path.display()
      )
    })?;

    if previous_mapping.as_ref() == Some(&dir_path) {
      dirs.insert(profile_id.to_string(), backup.clone());
    }
    backup_path = Some(backup);
  }

  let replacement = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    #[cfg(test)]
    maybe_inject_failure(FailurePoint::AfterBackupRename)?;

    std::fs::create_dir(&dir_path)
      .map_err(|e| format!("Failed to create ephemeral dir {}: {e}", dir_path.display()))?;

    #[cfg(test)]
    maybe_inject_failure(FailurePoint::AfterDirectoryCreation)?;

    dirs.insert(profile_id.to_string(), dir_path.clone());

    #[cfg(test)]
    maybe_inject_failure(FailurePoint::AfterRegistryInsert)?;

    Ok::<(), String>(())
  }));

  if replacement.as_ref().is_ok_and(Result::is_ok) {
    drop(dirs);

    if let Some(backup) = backup_path {
      if let Err(e) = remove_backup_dir(&backup) {
        remember_orphaned_backup(backup.clone());
        log::warn!(
          "Created ephemeral dir for profile {profile_id}, but failed to remove preserved backup {}: {e}; scheduled exact-path cleanup",
          backup.display()
        );
      }
    }

    log::info!(
      "Created ephemeral dir for profile {}: {}",
      profile_id,
      dir_path.display()
    );
    return Ok(dir_path);
  }

  let failure = match replacement {
    Ok(Err(error)) => TransactionFailure::Error(error),
    Err(payload) => TransactionFailure::Panic(payload),
    Ok(Ok(())) => unreachable!("successful replacements returned above"),
  };

  let rollback_result = rollback_ephemeral_replacement(
    profile_id,
    &dir_path,
    backup_path.as_deref(),
    previous_mapping,
    &mut dirs,
  );
  drop(dirs);

  match (failure, rollback_result) {
    (TransactionFailure::Error(error), Ok(())) => Err(error),
    (TransactionFailure::Error(error), Err(rollback_error)) => {
      Err(format!("{error}; rollback also failed: {rollback_error}"))
    }
    (TransactionFailure::Panic(payload), Ok(())) => std::panic::resume_unwind(payload),
    (TransactionFailure::Panic(payload), Err(rollback_error)) => {
      let panic_message = panic_payload_message(payload.as_ref());
      panic!("{panic_message}; rollback also failed: {rollback_error}");
    }
  }
}

enum TransactionFailure {
  Error(String),
  Panic(Box<dyn std::any::Any + Send>),
}

fn unique_backup_path(base: &Path, profile_id: &str) -> PathBuf {
  loop {
    let backup = base.join(format!(
      ".donut-ephemeral-backup-{profile_id}-{}",
      uuid::Uuid::new_v4()
    ));
    if !backup.exists() {
      return backup;
    }
  }
}

fn rename_ephemeral_dir(from: &Path, to: &Path) -> std::io::Result<()> {
  #[cfg(test)]
  maybe_inject_failure(FailurePoint::BackupRename).map_err(std::io::Error::other)?;

  std::fs::rename(from, to)
}

fn rollback_rename_ephemeral_dir(from: &Path, to: &Path) -> std::io::Result<()> {
  #[cfg(test)]
  maybe_inject_failure(FailurePoint::RollbackRename).map_err(std::io::Error::other)?;

  std::fs::rename(from, to)
}

fn rollback_ephemeral_replacement(
  profile_id: &str,
  dir_path: &Path,
  backup_path: Option<&Path>,
  previous_mapping: Option<PathBuf>,
  dirs: &mut HashMap<String, PathBuf>,
) -> Result<(), String> {
  if dir_path.exists() {
    if let Err(error) = std::fs::remove_dir_all(dir_path) {
      if let Some(backup) = backup_path.filter(|path| path.is_dir()) {
        dirs.insert(profile_id.to_string(), backup.to_path_buf());
      } else {
        restore_mapping(profile_id, previous_mapping, dirs);
      }
      return Err(format!(
        "failed to remove uncommitted replacement {}: {error}; mapping now points to {}",
        dir_path.display(),
        dirs.get(profile_id).map_or_else(
          || "no discoverable directory".to_string(),
          |path| path.display().to_string()
        )
      ));
    }
  }

  if let Some(backup) = backup_path {
    match rollback_rename_ephemeral_dir(backup, dir_path) {
      Ok(()) => restore_mapping(profile_id, previous_mapping, dirs),
      Err(error) => {
        if backup.is_dir() {
          dirs.insert(profile_id.to_string(), backup.to_path_buf());
        } else {
          dirs.remove(profile_id);
        }
        return Err(format!(
          "failed to restore backup {} to {}: {error}; mapping now points to {}",
          backup.display(),
          dir_path.display(),
          dirs.get(profile_id).map_or_else(
            || "no discoverable directory".to_string(),
            |path| path.display().to_string()
          )
        ));
      }
    }
  } else {
    restore_mapping(profile_id, previous_mapping, dirs);
  }

  Ok(())
}

fn restore_mapping(
  profile_id: &str,
  previous_mapping: Option<PathBuf>,
  dirs: &mut HashMap<String, PathBuf>,
) {
  if let Some(previous) = previous_mapping {
    dirs.insert(profile_id.to_string(), previous);
  } else {
    dirs.remove(profile_id);
  }
}

fn remove_backup_dir(backup_path: &Path) -> std::io::Result<()> {
  #[cfg(test)]
  if let Err(error) = maybe_inject_failure(FailurePoint::BackupDelete) {
    let mut entries = std::fs::read_dir(backup_path)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    if let Some(entry) = entries.into_iter().next() {
      let path = entry.path();
      if path.is_dir() {
        std::fs::remove_dir_all(path)?;
      } else {
        std::fs::remove_file(path)?;
      }
    }
    return Err(std::io::Error::other(error));
  }

  std::fs::remove_dir_all(backup_path)
}

fn remember_orphaned_backup(backup_path: PathBuf) {
  match ORPHANED_EPHEMERAL_BACKUPS.lock() {
    Ok(mut backups) => backups.push(backup_path),
    Err(e) => log::warn!(
      "Failed to schedule orphaned ephemeral backup cleanup because the cleanup list is unavailable: {e}"
    ),
  }
}

fn cleanup_orphaned_ephemeral_backups() {
  let backups = match ORPHANED_EPHEMERAL_BACKUPS.lock() {
    Ok(mut backups) => std::mem::take(&mut *backups),
    Err(e) => {
      log::warn!("Failed to access orphaned ephemeral backup cleanup list: {e}");
      return;
    }
  };

  for backup in backups {
    match std::fs::remove_dir_all(&backup) {
      Ok(()) => log::info!("Removed orphaned ephemeral backup: {}", backup.display()),
      Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
      Err(e) => {
        log::warn!(
          "Failed to remove orphaned ephemeral backup {}: {e}",
          backup.display()
        );
        remember_orphaned_backup(backup);
      }
    }
  }
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
  if let Some(message) = payload.downcast_ref::<String>() {
    message.clone()
  } else if let Some(message) = payload.downcast_ref::<&'static str>() {
    (*message).to_string()
  } else {
    "ephemeral directory replacement panicked".to_string()
  }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailurePoint {
  BackupRename,
  AfterBackupRename,
  AfterDirectoryCreation,
  AfterRegistryInsert,
  RollbackRename,
  BackupDelete,
  EphemeralDelete,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
enum FailureInjection {
  Error(FailurePoint),
  Panic(FailurePoint),
}

#[cfg(test)]
static FAILURE_INJECTIONS: Mutex<Vec<FailureInjection>> = Mutex::new(Vec::new());

#[cfg(test)]
fn set_failure_injection(injection: FailureInjection) {
  set_failure_injections(vec![injection]);
}

#[cfg(test)]
fn set_failure_injections(injections: Vec<FailureInjection>) {
  *FAILURE_INJECTIONS.lock().unwrap() = injections;
}

#[cfg(test)]
pub(crate) fn inject_ephemeral_delete_failure() {
  set_failure_injection(FailureInjection::Error(FailurePoint::EphemeralDelete));
}

#[cfg(test)]
fn maybe_inject_failure(point: FailurePoint) -> Result<(), String> {
  let injection = {
    let mut configured = FAILURE_INJECTIONS.lock().unwrap();
    configured
      .iter()
      .position(|injection| injection.point() == point)
      .map(|index| configured.remove(index))
  };

  match injection {
    Some(FailureInjection::Error(_)) => Err(format!("Injected failure at {point:?}")),
    Some(FailureInjection::Panic(_)) => panic!("Injected panic at {point:?}"),
    None => Ok(()),
  }
}

#[cfg(test)]
impl FailureInjection {
  fn point(self) -> FailurePoint {
    match self {
      Self::Error(point) | Self::Panic(point) => point,
    }
  }
}

pub fn get_ephemeral_dir(profile_id: &str) -> Option<PathBuf> {
  EPHEMERAL_DIRS.lock().ok()?.get(profile_id).cloned()
}

/// Removes the mapped directory and returns whether an existing directory was deleted.
///
/// A missing mapping or a mapping whose directory no longer exists returns `Ok(false)` after
/// removing any stale mapping. Other deletion failures retain the mapping so cleanup can be
/// retried against the same exact path.
pub fn remove_ephemeral_dir(profile_id: &str) -> Result<bool, String> {
  let mut dirs = EPHEMERAL_DIRS
    .lock()
    .map_err(|e| format!("Failed to lock ephemeral dirs: {e}"))?;
  let Some(dir_path) = dirs.get(profile_id).cloned() else {
    return Ok(false);
  };

  #[cfg(test)]
  if let Err(error) = maybe_inject_failure(FailurePoint::EphemeralDelete) {
    return Err(format!(
      "Failed to remove ephemeral dir {}: {error}",
      dir_path.display()
    ));
  }

  match std::fs::remove_dir_all(&dir_path) {
    Ok(()) => {
      dirs.remove(profile_id);
      log::info!(
        "Removed ephemeral dir for profile {}: {}",
        profile_id,
        dir_path.display()
      );
      Ok(true)
    }
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
      dirs.remove(profile_id);
      Ok(false)
    }
    Err(error) => Err(format!(
      "Failed to remove ephemeral dir {}: {error}",
      dir_path.display()
    )),
  }
}

/// Recover ephemeral dir mappings on startup by scanning the RAM-backed base dir.
/// Dir names are profile UUIDs, so we re-populate the in-memory HashMap.
/// Also cleans up old disk-based dirs from previous versions.
pub fn recover_ephemeral_dirs() {
  cleanup_legacy_dirs();

  let base = match get_ephemeral_base_dir() {
    Ok(base) => base,
    Err(e) => {
      log::warn!("Cannot recover ephemeral dirs: {e}");
      return;
    }
  };

  let entries = match std::fs::read_dir(&base) {
    Ok(entries) => entries,
    Err(_) => return,
  };

  let mut dirs = match EPHEMERAL_DIRS.lock() {
    Ok(dirs) => dirs,
    Err(_) => return,
  };

  for entry in entries.flatten() {
    if entry.path().is_dir() {
      if let Some(name) = entry.file_name().to_str() {
        if uuid::Uuid::parse_str(name).is_ok() {
          dirs.insert(name.to_string(), entry.path());
          log::info!("Recovered ephemeral dir for profile {}", name);
        }
      }
    }
  }
}

/// Remove old-format ephemeral dirs from /tmp (pre-tmpfs migration).
fn cleanup_legacy_dirs() {
  let temp_dir = std::env::temp_dir();
  let entries = match std::fs::read_dir(&temp_dir) {
    Ok(entries) => entries,
    Err(_) => return,
  };

  for entry in entries.flatten() {
    if let Some(name) = entry.file_name().to_str() {
      if name.starts_with("donut-ephemeral-") && entry.path().is_dir() {
        if let Err(e) = std::fs::remove_dir_all(entry.path()) {
          log::warn!("Failed to clean up legacy ephemeral dir: {e}");
        } else {
          log::info!(
            "Cleaned up legacy ephemeral dir: {}",
            entry.path().display()
          );
        }
      }
    }
  }
}

pub fn get_effective_profile_path(profile: &BrowserProfile, profiles_dir: &Path) -> PathBuf {
  if profile.ephemeral {
    if let Some(dir) = get_ephemeral_dir(&profile.id.to_string()) {
      return dir;
    }
  }
  profile.get_profile_data_path(profiles_dir)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn make_test_profile(id: uuid::Uuid, ephemeral: bool) -> BrowserProfile {
    BrowserProfile {
      id,
      name: "test".to_string(),
      browser: "camoufox".to_string(),
      version: "1.0".to_string(),
      proxy_id: None,
      vpn_id: None,
      launch_hook: None,
      process_id: None,
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
      ephemeral,
      extension_group_id: None,
      proxy_bypass_rules: Vec::new(),
      created_by_id: None,
      created_by_email: None,
      dns_blocklist: None,
    }
  }

  #[test]
  #[serial_test::serial]
  fn test_ephemeral_dir_lifecycle() {
    // Clear global state to avoid interference from other tests
    EPHEMERAL_DIRS.lock().unwrap().clear();

    let profile_id = uuid::Uuid::new_v4();
    let id_str = profile_id.to_string();

    let dir = create_ephemeral_dir(&id_str).unwrap();
    assert!(dir.is_dir());
    assert_eq!(get_ephemeral_dir(&id_str), Some(dir.clone()));

    let ephemeral_profile = make_test_profile(profile_id, true);
    let profiles_dir = std::env::temp_dir().join("test_profiles_ephemeral");
    assert_eq!(
      get_effective_profile_path(&ephemeral_profile, &profiles_dir),
      dir
    );

    remove_ephemeral_dir(&id_str).unwrap();
    assert!(!dir.exists());
    assert!(get_ephemeral_dir(&id_str).is_none());

    let persistent_profile = make_test_profile(uuid::Uuid::new_v4(), false);
    let expected = persistent_profile.get_profile_data_path(&profiles_dir);
    assert_eq!(
      get_effective_profile_path(&persistent_profile, &profiles_dir),
      expected
    );
  }

  #[test]
  #[serial_test::serial]
  fn test_remove_failure_keeps_mapping_and_directory_for_retry() {
    EPHEMERAL_DIRS.lock().unwrap().clear();
    let profile_id = uuid::Uuid::new_v4().to_string();
    let dir = create_ephemeral_dir(&profile_id).unwrap();
    std::fs::write(dir.join("marker"), b"retain on failed cleanup").unwrap();
    let concurrent_id = uuid::Uuid::new_v4().to_string();
    let concurrent_dir = create_ephemeral_dir(&concurrent_id).unwrap();
    std::fs::write(
      concurrent_dir.join("marker"),
      b"leave concurrent mapping alone",
    )
    .unwrap();

    set_failure_injection(FailureInjection::Error(FailurePoint::EphemeralDelete));
    let error = remove_ephemeral_dir(&profile_id).unwrap_err();

    assert!(error.contains("Injected failure at EphemeralDelete"));
    assert!(error.contains(&dir.display().to_string()));
    assert_eq!(get_ephemeral_dir(&profile_id), Some(dir.clone()));
    assert!(dir.is_dir());
    assert_eq!(
      std::fs::read(dir.join("marker")).unwrap(),
      b"retain on failed cleanup"
    );
    assert_eq!(
      get_ephemeral_dir(&concurrent_id),
      Some(concurrent_dir.clone())
    );
    assert_eq!(
      std::fs::read(concurrent_dir.join("marker")).unwrap(),
      b"leave concurrent mapping alone"
    );

    assert!(remove_ephemeral_dir(&profile_id).unwrap());
    assert!(!dir.exists());
    assert!(get_ephemeral_dir(&profile_id).is_none());
    remove_ephemeral_dir(&concurrent_id).unwrap();
    assert!(!concurrent_dir.exists());
  }

  #[test]
  #[serial_test::serial]
  fn test_recover_ephemeral_dirs() {
    let base = get_ephemeral_base_dir().unwrap();
    let test_id = uuid::Uuid::new_v4().to_string();
    let test_dir = base.join(&test_id);
    std::fs::create_dir_all(&test_dir).unwrap();

    // Clear the HashMap so recovery has something to find
    EPHEMERAL_DIRS.lock().unwrap().remove(&test_id);
    assert!(get_ephemeral_dir(&test_id).is_none());

    recover_ephemeral_dirs();
    assert_eq!(get_ephemeral_dir(&test_id), Some(test_dir.clone()));

    // Clean up
    remove_ephemeral_dir(&test_id).unwrap();
  }

  #[test]
  #[serial_test::serial]
  fn test_rename_failure_preserves_existing_dir_and_registry_state() {
    EPHEMERAL_DIRS.lock().unwrap().clear();
    let base = get_ephemeral_base_dir().unwrap();
    let profile_id = uuid::Uuid::new_v4().to_string();
    let old_dir = base.join(&profile_id);
    std::fs::create_dir_all(&old_dir).unwrap();
    std::fs::write(old_dir.join("old-marker"), b"keep until replaced").unwrap();

    let concurrent_id = uuid::Uuid::new_v4().to_string();
    let concurrent_dir = base.join(&concurrent_id);
    std::fs::create_dir_all(&concurrent_dir).unwrap();
    std::fs::write(concurrent_dir.join("concurrent-marker"), b"untouched").unwrap();

    {
      let mut dirs = EPHEMERAL_DIRS.lock().unwrap();
      dirs.insert(profile_id.clone(), old_dir.clone());
      dirs.insert(concurrent_id.clone(), concurrent_dir.clone());
    }

    set_failure_injection(FailureInjection::Error(FailurePoint::BackupRename));
    let error = create_ephemeral_dir(&profile_id).unwrap_err();

    assert!(error.contains("Failed to preserve existing ephemeral dir"));
    assert_eq!(get_ephemeral_dir(&profile_id), Some(old_dir.clone()));
    assert_eq!(
      std::fs::read(old_dir.join("old-marker")).unwrap(),
      b"keep until replaced"
    );
    assert_eq!(
      get_ephemeral_dir(&concurrent_id),
      Some(concurrent_dir.clone())
    );
    assert_eq!(
      std::fs::read(concurrent_dir.join("concurrent-marker")).unwrap(),
      b"untouched"
    );
    assert_eq!(EPHEMERAL_DIRS.lock().unwrap().len(), 2);

    remove_ephemeral_dir(&profile_id).unwrap();
    remove_ephemeral_dir(&concurrent_id).unwrap();
  }

  fn assert_rollback_preserves_concurrent_dir(injection: FailureInjection, expect_panic: bool) {
    EPHEMERAL_DIRS.lock().unwrap().clear();
    let base = get_ephemeral_base_dir().unwrap();
    let profile_id = uuid::Uuid::new_v4().to_string();
    let old_dir = base.join(&profile_id);
    std::fs::create_dir_all(&old_dir).unwrap();
    std::fs::write(old_dir.join("old-marker"), b"preserve").unwrap();
    let concurrent_id = uuid::Uuid::new_v4().to_string();
    let concurrent_dir = base.join(&concurrent_id);
    std::fs::create_dir_all(&concurrent_dir).unwrap();
    std::fs::write(concurrent_dir.join("marker"), b"keep").unwrap();
    {
      let mut dirs = EPHEMERAL_DIRS.lock().unwrap();
      dirs.insert(profile_id.clone(), old_dir.clone());
      dirs.insert(concurrent_id.clone(), concurrent_dir.clone());
    }

    set_failure_injection(injection);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
      create_ephemeral_dir(&profile_id)
    }));

    if expect_panic {
      assert!(result.is_err());
    } else {
      assert!(result.unwrap().is_err());
    }

    assert_eq!(get_ephemeral_dir(&profile_id), Some(old_dir.clone()));
    assert_eq!(
      std::fs::read(old_dir.join("old-marker")).unwrap(),
      b"preserve"
    );
    assert!(concurrent_dir.is_dir());
    assert_eq!(
      std::fs::read(concurrent_dir.join("marker")).unwrap(),
      b"keep"
    );
    assert_eq!(
      get_ephemeral_dir(&concurrent_id),
      Some(concurrent_dir.clone())
    );

    remove_ephemeral_dir(&profile_id).unwrap();
    remove_ephemeral_dir(&concurrent_id).unwrap();
    assert!(!concurrent_dir.exists());
    FAILURE_INJECTIONS.lock().unwrap().clear();
    ORPHANED_EPHEMERAL_BACKUPS.lock().unwrap().clear();
  }

  #[test]
  #[serial_test::serial]
  fn test_failure_after_backup_rename_restores_old_profile() {
    assert_rollback_preserves_concurrent_dir(
      FailureInjection::Error(FailurePoint::AfterBackupRename),
      false,
    );
  }

  #[test]
  #[serial_test::serial]
  fn test_panic_after_backup_rename_restores_old_profile() {
    assert_rollback_preserves_concurrent_dir(
      FailureInjection::Panic(FailurePoint::AfterBackupRename),
      true,
    );
  }

  #[test]
  #[serial_test::serial]
  fn test_creation_failure_after_directory_creation_rolls_back_exact_dir() {
    assert_rollback_preserves_concurrent_dir(
      FailureInjection::Error(FailurePoint::AfterDirectoryCreation),
      false,
    );
  }

  #[test]
  #[serial_test::serial]
  fn test_creation_panic_after_directory_creation_rolls_back_exact_dir() {
    assert_rollback_preserves_concurrent_dir(
      FailureInjection::Panic(FailurePoint::AfterDirectoryCreation),
      true,
    );
  }

  #[test]
  #[serial_test::serial]
  fn test_creation_failure_after_registry_insert_rolls_back_dir_and_entry() {
    assert_rollback_preserves_concurrent_dir(
      FailureInjection::Error(FailurePoint::AfterRegistryInsert),
      false,
    );
  }

  #[test]
  #[serial_test::serial]
  fn test_creation_panic_after_registry_insert_rolls_back_dir_and_entry() {
    assert_rollback_preserves_concurrent_dir(
      FailureInjection::Panic(FailurePoint::AfterRegistryInsert),
      true,
    );
  }

  #[test]
  #[serial_test::serial]
  fn test_rollback_rename_failure_keeps_mapping_on_discoverable_backup() {
    EPHEMERAL_DIRS.lock().unwrap().clear();
    ORPHANED_EPHEMERAL_BACKUPS.lock().unwrap().clear();
    let base = get_ephemeral_base_dir().unwrap();
    let profile_id = uuid::Uuid::new_v4().to_string();
    let old_dir = base.join(&profile_id);
    std::fs::create_dir_all(&old_dir).unwrap();
    std::fs::write(old_dir.join("old-marker"), b"discoverable").unwrap();
    EPHEMERAL_DIRS
      .lock()
      .unwrap()
      .insert(profile_id.clone(), old_dir.clone());

    set_failure_injections(vec![
      FailureInjection::Error(FailurePoint::AfterDirectoryCreation),
      FailureInjection::Error(FailurePoint::RollbackRename),
    ]);
    let error = create_ephemeral_dir(&profile_id).unwrap_err();

    assert!(error.contains("rollback also failed"));
    let discoverable_backup = get_ephemeral_dir(&profile_id).unwrap();
    assert_ne!(discoverable_backup, old_dir);
    assert!(!old_dir.exists());
    assert!(discoverable_backup.is_dir());
    assert_eq!(
      std::fs::read(discoverable_backup.join("old-marker")).unwrap(),
      b"discoverable"
    );

    remove_ephemeral_dir(&profile_id).unwrap();
    FAILURE_INJECTIONS.lock().unwrap().clear();
  }

  #[test]
  #[serial_test::serial]
  fn test_backup_delete_partial_failure_commits_new_profile_and_schedules_cleanup() {
    EPHEMERAL_DIRS.lock().unwrap().clear();
    ORPHANED_EPHEMERAL_BACKUPS.lock().unwrap().clear();
    let base = get_ephemeral_base_dir().unwrap();
    let profile_id = uuid::Uuid::new_v4().to_string();
    let old_dir = base.join(&profile_id);
    std::fs::create_dir_all(&old_dir).unwrap();
    std::fs::write(old_dir.join("a-deleted-first"), b"partial").unwrap();
    std::fs::write(old_dir.join("z-old-marker"), b"preserved orphan").unwrap();
    EPHEMERAL_DIRS
      .lock()
      .unwrap()
      .insert(profile_id.clone(), old_dir.clone());

    set_failure_injection(FailureInjection::Error(FailurePoint::BackupDelete));
    let new_dir = create_ephemeral_dir(&profile_id).unwrap();

    assert_eq!(new_dir, old_dir);
    assert_eq!(get_ephemeral_dir(&profile_id), Some(new_dir.clone()));
    assert!(new_dir.is_dir());
    assert!(!new_dir.join("a-deleted-first").exists());
    assert!(!new_dir.join("z-old-marker").exists());

    let orphan = ORPHANED_EPHEMERAL_BACKUPS
      .lock()
      .unwrap()
      .first()
      .cloned()
      .unwrap();
    assert!(orphan.is_dir());
    assert!(!orphan.join("a-deleted-first").exists());
    assert_eq!(
      std::fs::read(orphan.join("z-old-marker")).unwrap(),
      b"preserved orphan"
    );

    cleanup_orphaned_ephemeral_backups();
    assert!(!orphan.exists());
    assert!(ORPHANED_EPHEMERAL_BACKUPS.lock().unwrap().is_empty());
    assert_eq!(get_ephemeral_dir(&profile_id), Some(new_dir));

    remove_ephemeral_dir(&profile_id).unwrap();
  }

  #[test]
  #[serial_test::serial]
  fn test_success_replaces_profile_and_cleans_backup() {
    EPHEMERAL_DIRS.lock().unwrap().clear();
    ORPHANED_EPHEMERAL_BACKUPS.lock().unwrap().clear();
    let base = get_ephemeral_base_dir().unwrap();
    let profile_id = uuid::Uuid::new_v4().to_string();
    let old_dir = base.join(&profile_id);
    std::fs::create_dir_all(&old_dir).unwrap();
    std::fs::write(old_dir.join("old-marker"), b"remove after commit").unwrap();
    EPHEMERAL_DIRS
      .lock()
      .unwrap()
      .insert(profile_id.clone(), old_dir.clone());

    let new_dir = create_ephemeral_dir(&profile_id).unwrap();

    assert_eq!(new_dir, old_dir);
    assert_eq!(get_ephemeral_dir(&profile_id), Some(new_dir.clone()));
    assert!(new_dir.is_dir());
    assert!(!new_dir.join("old-marker").exists());
    assert!(ORPHANED_EPHEMERAL_BACKUPS.lock().unwrap().is_empty());
    let backup_prefix = format!(".donut-ephemeral-backup-{profile_id}-");
    assert!(std::fs::read_dir(&base)
      .unwrap()
      .filter_map(Result::ok)
      .all(|entry| !entry
        .file_name()
        .to_string_lossy()
        .starts_with(&backup_prefix)));

    remove_ephemeral_dir(&profile_id).unwrap();
  }
}

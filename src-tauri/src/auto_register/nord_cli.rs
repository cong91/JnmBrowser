//! NordVPN Windows CLI wrapper for system-wide IP rotation during auto-reg.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Build `NordVPN.exe` connect args (`-c`, optional `-n` or `-g`).
pub fn connect_args(group: Option<&str>, server_name: Option<&str>) -> Vec<String> {
  let mut args = vec!["-c".to_string()];
  if let Some(name) = server_name.map(str::trim).filter(|s| !s.is_empty()) {
    args.push("-n".into());
    args.push(name.to_string());
  } else if let Some(g) = group.map(str::trim).filter(|s| !s.is_empty()) {
    args.push("-g".into());
    args.push(g.to_string());
  }
  args
}

/// Build disconnect args.
pub fn disconnect_args() -> Vec<String> {
  vec!["-d".to_string()]
}

/// Default install path on Windows; `nordvpn` elsewhere (not supported in v1).
pub fn default_cli_path() -> PathBuf {
  #[cfg(windows)]
  {
    PathBuf::from(r"C:\Program Files\NordVPN\NordVPN.exe")
  }
  #[cfg(not(windows))]
  {
    PathBuf::from("nordvpn")
  }
}

/// Resolve CLI path from optional override or default install location.
pub fn resolve_cli_path(override_path: Option<&str>) -> Result<PathBuf, String> {
  if let Some(p) = override_path.map(str::trim).filter(|s| !s.is_empty()) {
    let pb = PathBuf::from(p);
    if pb.is_file() {
      return Ok(pb);
    }
    return Err(format!("NordVPN CLI not found at override path: {p}"));
  }
  let def = default_cli_path();
  if def.is_file() {
    return Ok(def);
  }
  Err(format!(
    "NordVPN CLI not found at {}. Install NordVPN or set nordCliPath.",
    def.display()
  ))
}

fn run_nord(exe: &Path, args: &[String]) -> Result<String, String> {
  let mut cmd = Command::new(exe);
  cmd.args(args);
  #[cfg(windows)]
  {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
  }
  let output = cmd
    .output()
    .map_err(|e| format!("failed to spawn NordVPN CLI: {e}"))?;
  let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
  let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
  if !output.status.success() {
    return Err(format!(
      "NordVPN CLI {args:?} failed (status {:?}): {stderr} {stdout}",
      output.status.code()
    ));
  }
  Ok(if stdout.is_empty() { stderr } else { stdout })
}

/// Disconnect NordVPN (system-wide).
pub fn disconnect(cli_path: Option<&str>) -> Result<(), String> {
  let exe = resolve_cli_path(cli_path)?;
  let _ = run_nord(&exe, &disconnect_args())?;
  Ok(())
}

/// Connect NordVPN to best / group / named server.
pub fn connect(
  cli_path: Option<&str>,
  group: Option<&str>,
  server_name: Option<&str>,
) -> Result<(), String> {
  let exe = resolve_cli_path(cli_path)?;
  let _ = run_nord(&exe, &connect_args(group, server_name))?;
  Ok(())
}

/// Disconnect then connect for IP rotation.
pub fn rotate(
  cli_path: Option<&str>,
  group: Option<&str>,
  server_name: Option<&str>,
) -> Result<(), String> {
  // Best-effort disconnect even if already down.
  let _ = disconnect(cli_path);
  std::thread::sleep(Duration::from_secs(2));
  connect(cli_path, group, server_name)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn connect_args_best() {
    assert_eq!(connect_args(None, None), vec!["-c".to_string()]);
  }

  #[test]
  fn connect_args_group() {
    assert_eq!(
      connect_args(Some("United States"), None),
      vec![
        "-c".to_string(),
        "-g".to_string(),
        "United States".to_string()
      ]
    );
  }

  #[test]
  fn connect_args_server_wins_over_group() {
    assert_eq!(
      connect_args(Some("United States"), Some("United States #5")),
      vec![
        "-c".to_string(),
        "-n".to_string(),
        "United States #5".to_string()
      ]
    );
  }

  #[test]
  fn disconnect_args_ok() {
    assert_eq!(disconnect_args(), vec!["-d".to_string()]);
  }

  #[test]
  fn default_cli_path_windows_shape() {
    let p = default_cli_path();
    assert!(p.to_string_lossy().contains("NordVPN"));
  }
}

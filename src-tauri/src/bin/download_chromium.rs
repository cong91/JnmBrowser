//! One-shot binary to download and install the latest fingerprint-chromium.
//!
//! ```text
//! cargo run --manifest-path src-tauri/Cargo.toml --bin download-chromium
//! ```

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

/// Read the local fingerprint-chromium.json manifest to get the version and download URL.
fn load_manifest() -> Result<(String, String), String> {
  // Try several locations for the manifest
  let candidates = vec![
    PathBuf::from("fingerprint-chromium.json"),
    PathBuf::from("../fingerprint-chromium.json"),
  ];

  for path in candidates {
    if path.exists() {
      let content = fs::read_to_string(&path).map_err(|e| format!("read {path:?}: {e}"))?;
      let parsed: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| format!("parse {path:?}: {e}"))?;
      let version = parsed["version"].as_str().unwrap_or_default().to_string();

      // Pick the right platform key
      let platform_key = if cfg!(target_os = "windows") {
        "windows-x64"
      } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
          "macos-arm64"
        } else {
          "macos-x64"
        }
      } else {
        "linux-x64"
      };

      let url = parsed["downloads"][platform_key]
        .as_str()
        .ok_or_else(|| format!("No download URL for platform {platform_key}"))?
        .to_string();

      return Ok((version, url));
    }
  }

  Err("fingerprint-chromium.json not found".into())
}

fn data_dir() -> PathBuf {
  if let Ok(dir) =
    std::env::var("JNMBROWSER_DATA_DIR").or_else(|_| std::env::var("DONUTBROWSER_DATA_DIR"))
  {
    return PathBuf::from(dir);
  }

  // Check portable
  if let Ok(exe) = std::env::current_exe() {
    if let Some(parent) = exe.parent() {
      let portable_marker = parent.join(".portable");
      if portable_marker.exists() {
        return parent.join("data");
      }
    }
  }

  dirs::data_local_dir()
    .unwrap_or_else(|| PathBuf::from("."))
    .join("JnmBrowser")
}

fn binaries_dir() -> PathBuf {
  data_dir().join("binaries")
}

fn registry_path() -> PathBuf {
  data_dir().join("data").join("downloaded_browsers.json")
}

#[tokio::main]
async fn main() {
  println!("=== JnmBrowser Chromium Downloader ===\n");

  let (version, url) = match load_manifest() {
    Ok(v) => v,
    Err(e) => {
      eprintln!("Failed to load manifest: {e}");
      std::process::exit(1);
    }
  };

  println!("Version: {version}");
  println!("Download URL: {url}");
  println!();

  let install_dir = binaries_dir().join("fingerprint-chromium").join(&version);

  // Check if already installed
  if install_dir.exists() {
    let has_exe = ["chrome.exe", "chromium.exe", "Chromium.app", "chrome"]
      .iter()
      .any(|name| {
        install_dir.join(name).exists()
          || install_dir.join("bin").join(name).exists()
          || install_dir.join("chrome-win").join(name).exists()
      });

    if has_exe {
      println!(
        "Chromium {version} is already installed at {}",
        install_dir.display()
      );
      println!("Nothing to do.");
      return;
    }
    // Directory exists but no executable — clean it up
    println!(
      "Removing incomplete installation at {}",
      install_dir.display()
    );
    let _ = fs::remove_dir_all(&install_dir);
  }

  // Create directories
  fs::create_dir_all(&install_dir).unwrap_or_else(|e| {
    eprintln!("Failed to create directory {}: {e}", install_dir.display());
    std::process::exit(1);
  });

  // Download
  println!("Downloading...");
  let client = reqwest::Client::builder()
    .timeout(Duration::from_secs(600))
    .build()
    .unwrap();

  let response = client
    .get(&url)
    .header(
      "User-Agent",
      "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36",
    )
    .send()
    .await
    .unwrap_or_else(|e| {
      eprintln!("Download failed: {e}");
      std::process::exit(1);
    });

  let total_size = response.content_length().unwrap_or(0);
  let archive_path = install_dir.join(url.rsplit('/').next().unwrap_or("chromium.zip"));

  let bytes = response.bytes().await.unwrap_or_else(|e| {
    eprintln!("Download read failed: {e}");
    std::process::exit(1);
  });

  println!("Downloaded {} bytes", bytes.len());
  if total_size > 0 {
    println!("  ({:.1} MB)", total_size as f64 / 1_048_576.0);
  }

  fs::write(&archive_path, &bytes).unwrap_or_else(|e| {
    eprintln!("Failed to write archive: {e}");
    std::process::exit(1);
  });

  // Extract
  println!("Extracting...");
  let file = fs::File::open(&archive_path).unwrap();
  let mut archive = zip::ZipArchive::new(file).unwrap_or_else(|e| {
    eprintln!("Failed to open zip: {e}");
    std::process::exit(1);
  });

  for i in 0..archive.len() {
    let mut entry = archive.by_index(i).unwrap();
    let name = entry.mangled_name();
    let out_path = install_dir.join(&name);

    if entry.is_dir() {
      let _ = fs::create_dir_all(&out_path);
    } else {
      if let Some(parent) = out_path.parent() {
        let _ = fs::create_dir_all(parent);
      }
      let mut out_file = fs::File::create(&out_path).unwrap_or_else(|e| {
        eprintln!("Failed to create file {}: {e}", out_path.display());
        std::process::exit(1);
      });
      std::io::copy(&mut entry, &mut out_file).unwrap();
    }
  }

  println!("Extracted to {}", install_dir.display());

  // Flatten: if the zip contained everything in a single top-level folder,
  // move its contents up to the install directory (matches the app's expected layout).
  {
    let mut top_dirs: Vec<PathBuf> = Vec::new();
    let mut top_files: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = fs::read_dir(&install_dir) {
      for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
          top_dirs.push(p);
        } else {
          top_files.push(p);
        }
      }
    }
    // Only flatten when there is exactly one directory and few files (archive extras).
    if top_dirs.len() == 1 && top_files.len() <= 2 {
      let sub = &top_dirs[0];
      println!("Flattening {} -> {}", sub.display(), install_dir.display());
      if let Ok(entries) = fs::read_dir(sub) {
        for entry in entries.flatten() {
          let src = entry.path();
          let name = src.file_name().unwrap();
          let dst = install_dir.join(name);
          let _ = fs::rename(&src, &dst);
        }
      }
      let _ = fs::remove_dir(sub);
    }
  }

  // Delete archive
  let _ = fs::remove_file(&archive_path);

  // Verify
  let exe_found = ["chrome.exe", "chromium.exe", "Chromium.app", "chrome"]
    .iter()
    .any(|name| {
      install_dir.join(name).exists()
        || install_dir.join("bin").join(name).exists()
        || install_dir.join("chrome-win").join(name).exists()
    });

  if !exe_found {
    // List directory contents for debugging
    println!("Warning: No executable found. Directory contents:");
    if let Ok(entries) = fs::read_dir(&install_dir) {
      for entry in entries.flatten() {
        println!("  {}", entry.path().display());
      }
    }
  }

  // Update registry
  let reg_path = registry_path();
  if let Some(parent) = reg_path.parent() {
    let _ = fs::create_dir_all(parent);
  }

  let mut registry: serde_json::Value = if reg_path.exists() {
    let content = fs::read_to_string(&reg_path).unwrap_or_default();
    serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({"browsers": {}}))
  } else {
    serde_json::json!({"browsers": {}})
  };

  registry["browsers"]["chromium"] = serde_json::json!({
    &version: {
      "browser": "chromium",
      "version": &version,
      "file_path": install_dir.to_string_lossy()
    }
  });

  let reg_content = serde_json::to_string_pretty(&registry).unwrap();
  fs::write(&reg_path, reg_content).unwrap_or_else(|e| {
    eprintln!("Failed to write registry: {e}");
    std::process::exit(1);
  });

  println!("\n✅ Chromium {version} installed successfully!");
  println!("   Location: {}", install_dir.display());
  println!("   Registry: {}", reg_path.display());
}

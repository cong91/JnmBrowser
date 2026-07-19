import { execSync, execFileSync } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const MANIFEST_DIR = dirname(fileURLToPath(import.meta.url));
const PROFILE = process.env.PROFILE || "debug";

/** Processes that commonly lock sidecar binaries on Windows during tauri-build/copy. */
const WINDOWS_LOCKERS = [
  "donut-proxy.exe",
  "donut-daemon.exe",
  "JnmBrowser.exe",
  "auto-login-live.exe",
];

function getTarget() {
  if (process.env.TARGET) return process.env.TARGET;
  try {
    const output = execSync("rustc -vV", { encoding: "utf-8" });
    const match = output.match(/host:\s*(.+)/);
    if (match) return match[1].trim();
  } catch {}
  return "unknown";
}

function getHostTarget() {
  try {
    const output = execSync("rustc -vV", { encoding: "utf-8" });
    const match = output.match(/host:\s*(.+)/);
    if (match) return match[1].trim();
  } catch {}
  return "unknown";
}

const TARGET = getTarget();
const HOST_TARGET = getHostTarget();
const isWindows = TARGET.includes("windows") || process.platform === "win32";

function sleepMs(ms) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

/**
 * Force-stop processes that hold locks on sidecar binaries.
 * Safe no-op when processes are not running.
 */
function unlockSidecars() {
  if (!isWindows) return;

  const killed = [];
  for (const image of WINDOWS_LOCKERS) {
    try {
      // /T kills the process tree; ignore non-zero when process is absent.
      execSync(`taskkill /F /T /IM ${image}`, {
        stdio: "ignore",
        windowsHide: true,
      });
      killed.push(image);
    } catch {
      // Process not running — fine.
    }
  }

  if (killed.length > 0) {
    console.log(
      `Unlocked sidecar targets (stopped: ${killed.join(", ")}). Waiting briefly for file handles...`,
    );
    sleepMs(400);
  }
}

function copyWithRetry(source, dest, label) {
  const maxAttempts = 3;
  let lastError;

  for (let attempt = 1; attempt <= maxAttempts; attempt++) {
    try {
      copyFileSync(source, dest);
      return;
    } catch (err) {
      lastError = err;
      const code = err && err.code;
      const isLock =
        code === "EPERM" ||
        code === "EACCES" ||
        code === "EBUSY" ||
        /permission denied|being used by another process/i.test(String(err));

      if (!isLock || attempt === maxAttempts) break;

      console.warn(
        `Copy locked (${label}, attempt ${attempt}/${maxAttempts}): ${err.message || err}`,
      );
      unlockSidecars();
      sleepMs(300 * attempt);
    }
  }

  const hint = isWindows
    ? ` File may still be locked. Tried taskkill on: ${WINDOWS_LOCKERS.join(", ")}.`
    : "";
  console.error(
    `Error: Failed to copy ${label} → ${dest}: ${lastError?.message || lastError}.${hint}`,
  );
  process.exit(1);
}

// Determine source directory
let srcDir;
if (TARGET === HOST_TARGET || TARGET === "unknown") {
  srcDir = join(MANIFEST_DIR, "target", PROFILE === "release" ? "release" : "debug");
} else {
  srcDir = join(MANIFEST_DIR, "target", TARGET, PROFILE === "release" ? "release" : "debug");
}

const destDir = join(MANIFEST_DIR, "binaries");
mkdirSync(destDir, { recursive: true });

// Always try to free sidecar locks before copy/build on Windows.
unlockSidecars();

function copyBinary(baseName) {
  const binName = isWindows ? `${baseName}.exe` : baseName;
  const source = join(srcDir, binName);

  let destName = `${baseName}-${TARGET}`;
  if (isWindows) destName += ".exe";
  const dest = join(destDir, destName);

  if (existsSync(source)) {
    copyWithRetry(source, dest, binName);
    console.log(`Copied ${binName} to ${dest}`);
  } else {
    console.log(`Warning: Binary not found at ${source}`);
    console.log(`Building ${baseName} binary...`);

    const buildArgs = ["build", "--bin", baseName];
    if (PROFILE === "release") buildArgs.push("--release");
    if (TARGET !== "unknown" && TARGET !== HOST_TARGET) {
      buildArgs.push("--target", TARGET);
    }

    try {
      execFileSync("cargo", buildArgs, {
        cwd: MANIFEST_DIR,
        stdio: "inherit",
      });
    } catch (err) {
      // cargo may fail if target/*.exe is locked by a running process
      if (isWindows) {
        console.warn(
          `cargo build --bin ${baseName} failed; retrying after unlock...`,
        );
        unlockSidecars();
        sleepMs(500);
        execFileSync("cargo", buildArgs, {
          cwd: MANIFEST_DIR,
          stdio: "inherit",
        });
      } else {
        throw err;
      }
    }

    if (existsSync(source)) {
      copyWithRetry(source, dest, binName);
      console.log(`Built and copied ${binName} to ${dest}`);
    } else {
      console.error(`Error: Failed to build ${baseName} binary`);
      process.exit(1);
    }
  }
}

copyBinary("donut-proxy");
copyBinary("donut-daemon");

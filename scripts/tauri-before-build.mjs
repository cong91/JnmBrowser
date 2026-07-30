import { execFileSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const pnpm = process.platform === "win32" ? "pnpm.cmd" : "pnpm";
const useShell = process.platform === "win32";

const env = {
  ...process.env,
  PROFILE: process.env.PROFILE || "release",
};

// Stop any running sidecars and copy the current outputs first. This releases
// Windows file locks before Cargo tries to replace the executables.
execFileSync(pnpm, ["copy-proxy-binary"], {
  cwd: repoRoot,
  env,
  shell: useShell,
  stdio: "inherit",
});

// Tauri builds all package binaries, but externalBin is read from binaries/.
// Build the sidecars first, then sync them so the current build packages the
// newly compiled bytes instead of the previous build's outputs.
const cargoArgs = [
  "build",
  "--bin",
  "donut-proxy",
  "--bin",
  "donut-daemon",
];
if (env.PROFILE === "release") cargoArgs.push("--release");
if (process.env.TARGET) cargoArgs.push("--target", process.env.TARGET);

execFileSync("cargo", cargoArgs, {
  cwd: join(repoRoot, "src-tauri"),
  env,
  stdio: "inherit",
});

execFileSync(pnpm, ["copy-proxy-binary"], {
  cwd: repoRoot,
  env,
  shell: useShell,
  stdio: "inherit",
});

execFileSync(pnpm, ["exec", "next", "build"], {
  cwd: repoRoot,
  env,
  shell: useShell,
  stdio: "inherit",
});

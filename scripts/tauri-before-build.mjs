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

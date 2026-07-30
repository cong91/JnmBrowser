// Work around Tauri CLI 2.10.x NSIS bundler bug where it incorrectly
// resolves the project-level `.zcode` directory as `target/release/.zcode.exe`
// and fails `metadata()` if the file is absent.
//
// This placeholder is not included in the installer. Development harnesses
// live outside src/bin so Tauri does not discover or package them.

import { copyFileSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const releaseDir = join(repoRoot, "src-tauri", "target", "release");
const mainBinary = join(releaseDir, "JnmBrowser.exe");
const zcodePlaceholder = join(releaseDir, ".zcode.exe");

if (!existsSync(zcodePlaceholder) && existsSync(mainBinary)) {
  copyFileSync(mainBinary, zcodePlaceholder);
  console.log("[tauri-before-bundle] Created .zcode.exe placeholder for NSIS bundler");
}

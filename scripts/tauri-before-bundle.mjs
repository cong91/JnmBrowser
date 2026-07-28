// Work around Tauri CLI 2.10.x NSIS bundler bug where it incorrectly
// resolves the .zcode directory (project-level ZCode agent data) as a
// binary file named .zcode.exe in the target/release directory.
//
// The bundler tries `std::fs::metadata("target/release/.zcode.exe")` and
// fails with os error 2 if the file is absent.
//
// Fix: create a tiny valid PE placeholder so the bundler can stat it.
// This file is NOT included in the final installer — it only satisfies
// the bundler's file-existence check.

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

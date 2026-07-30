#!/usr/bin/env node
// Verify all locale files in src/i18n/locales share the exact same key set
// and that no non-English locale contains empty-string values.
// Usage (from repo root): node .claude/skills/jnm-i18n/check-locales.mjs
import fs from "node:fs";
import path from "node:path";

const localesDir = path.resolve(process.cwd(), "src/i18n/locales");
const locales = ["en", "es", "fr", "ja", "pt", "ru", "zh"];

const flatten = (obj, prefix = "") =>
  Object.entries(obj).flatMap(([k, v]) =>
    v && typeof v === "object"
      ? flatten(v, `${prefix}${k}.`)
      : [`${prefix}${k}`],
  );

const load = (locale) =>
  JSON.parse(
    fs.readFileSync(path.join(localesDir, `${locale}.json`), "utf8"),
  );

const trees = Object.fromEntries(locales.map((l) => [l, load(l)]));
const keySets = Object.fromEntries(
  locales.map((l) => [l, new Set(flatten(trees[l]))]),
);

let failed = false;
const enKeys = keySets.en;

for (const locale of locales.slice(1)) {
  const missing = [...enKeys].filter((k) => !keySets[locale].has(k));
  const extra = [...keySets[locale]].filter((k) => !enKeys.has(k));
  if (missing.length || extra.length) {
    failed = true;
    if (missing.length)
      console.error(`[${locale}] missing keys: ${missing.join(", ")}`);
    if (extra.length)
      console.error(`[${locale}] extra keys:   ${extra.join(", ")}`);
  }
}

const empties = (obj, prefix = "") =>
  Object.entries(obj).flatMap(([k, v]) =>
    v && typeof v === "object"
      ? empties(v, `${prefix}${k}.`)
      : v === ""
        ? [`${prefix}${k}`]
        : [],
  );

for (const locale of locales.slice(1)) {
  const empty = empties(trees[locale]);
  if (empty.length) {
    failed = true;
    console.error(`[${locale}] empty values: ${empty.join(", ")}`);
  }
}

if (failed) process.exit(1);
console.log(`OK: ${locales.length} locales, ${enKeys.size} keys each`);

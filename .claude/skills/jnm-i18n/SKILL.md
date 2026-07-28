---
name: jnm-i18n
description: Mandatory translation workflow for any user-facing string in JnmBrowser (7 locales, no fallbacks, no empty values). Use whenever adding or editing JSX text, toasts, dialogs, buttons, placeholders, table headers, tooltips, or empty states.
---

# JnmBrowser i18n (mandatory)

Every user-facing string must go through `t("namespace.key")` from `useTranslation()` (react-i18next). Raw English literals in rendered UI are forbidden.

## Hard rules

1. **No raw literals** in JSX, toasts, dialogs, buttons, placeholders, table headers, tooltips, empty states. Import `useTranslation` when missing.
2. **Never** `t(key, "fallback")` — fallbacks hide missing translations. The key must exist in every locale before the call site lands.
3. **All seven locales** in `src/i18n/locales/`: `en`, `es`, `fr`, `ja`, `pt`, `ru`, `zh`. English-only is incomplete.
4. **No empty-string values** in non-English locales.
5. **Prefer one interpolated key** — `t("foo.bar", { name })` — over splitting prefix/suffix into two keys.
6. Exempt: `console.log/warn/error`, dev-only labels, internal IDs, CSS classes, type names.

## Workflow

1. **Reuse first.** Check `src/i18n/locales/en.json` for an existing key before creating one. Existing namespaces:
   `common` (incl. `common.buttons.*`, `common.labels.*`), `settings`, `header`, `profiles`, `createProfile`, `deleteDialog`, `proxies`, `groups`, `sync`, `integrations`, `import`, `config`, `cookies`, `toasts`, `errors`, `browser`, `fingerprint`, `warnings`, `syncAll`, `crossOs`, `profileInfo`, `extensions`, `pro`, `dnsBlocklist`, `vpns`, `importProfile`, `syncTooltips`, `groupManagement`, `proxyAssignment`, `groupAssignment`, `profileSelector`, `locationProxy`, `launchOnLogin`, `wayfernTerms`, `commercialTrial`, `permissionDialog`, `traffic`, `camoufoxDialog`, `proxyCheck`, `vpnCheck`, `profileTable`, `releaseTypeSelector`, `dataTableActionBar`, `appUpdate`, `browserDownload`, `versionUpdater`, `browserSupportWarning`, `recorder`, `registration`, `sms`, `autoLogin`.
2. Add the key to **en.json first**, then translate into the other six files at the identical path.
3. Wire the call site: `const { t } = useTranslation();` then `t("namespace.key")`. For hooks that can't use the hook form, the codebase imports `i18n` from `@/i18n` and calls `i18n.t(...)` (see `src/hooks/use-profile-events.ts`).
4. **Validate** before finishing:

```powershell
node .claude/skills/jnm-i18n/check-locales.mjs
```

It fails if any locale is missing/extra keys or contains empty strings.

## Renaming/removing keys

Update all 7 files in the same change and re-run the checker. Grep the whole `src/` tree for the old key — it may be referenced from hooks or lib code, not just components.

## Example

```tsx
const { t } = useTranslation();
// ...
toast.success(t("proxyCheck.success", { country: result.country }));
<Button>{t("common.buttons.save")}</Button>
```

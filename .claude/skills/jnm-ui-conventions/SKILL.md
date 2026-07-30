---
name: jnm-ui-conventions
description: JnmBrowser frontend UI conventions — theme CSS variables instead of hardcoded Tailwind colors, shadcn/ui usage, dialog/action-item/table patterns. Use whenever creating or modifying components under src/.
---

# UI conventions

## Theming — never hardcode colors

Forbidden: `text-red-500`, `bg-green-600`, any raw Tailwind palette class. Use semantic theme variables from `src/lib/themes.ts`:

- Classes: `background`, `foreground`, `card`, `popover`, `primary`, `secondary`, `muted`, `accent`, `destructive`, `success`, `warning`, `border`, `chart-1`…`chart-5` (plus `*-foreground` pairs).
- Usage: `bg-success`, `text-destructive`, `border-warning`, opacity variants like `bg-destructive/10`.

## Components

- shadcn/ui primitives live in `src/components/ui/`. Add new ones only via `pnpm shadcn:add <name>` (config in `components.json`) — don't hand-write new primitives.
- Path aliases: `@/components`, `@/components/ui`, `@/hooks`, `@/lib`.
- Client components start with `"use client";` (see any existing dialog).
- Toasts: `sonner` (`toast.success/error`) — strings always translated (`jnm-i18n`). Styled variants go through `custom-toast.tsx` / `app-update-toast.tsx` patterns.
- Async buttons: reuse `loading-button.tsx` rather than hand-rolling spinner state.
- Country flags: `flag-icon.tsx` (`flag-icons` package), not emoji.

## Established patterns to mirror (don't invent parallels)

- **Dialogs**: one file per dialog in `src/components/*-dialog.tsx`, built on `@/components/ui/dialog`. Copy the closest sibling (e.g. `proxy-form-dialog.tsx`, `delete-confirmation-dialog.tsx`) for structure, open/close props, and footer button layout.
- **Profile action items**: use the existing `ActionItem { icon, label, onClick, disabled, hidden }` shape and extend the Launch-with-Sync / Launch-with-Record patterns for chromium/camoufox — never build a separate parallel control for the same action.
- **Tables**: `profile-data-table.tsx` / `@tanstack/react-table`. The bulk-selection table **already has a checkbox column** — do not add a second checkbox for record/select flows.
- **Sorting/state**: `use-table-sorting.ts`, `use-controlled-state.tsx`.

## Strings

Everything user-visible is translated — see `jnm-i18n`. Table headers, tooltips, empty states included.

## Icons

`lucide-react` and `react-icons` (e.g. `react-icons/fi`) are in use; match the icon family of the surrounding UI. Custom brand icons live in `src/components/icons/`.

## Styling mechanics

- Tailwind v4 (`@tailwindcss/postcss`) + `tw-animate-css`; global entry in `src/styles/`.
- Merge class names with `cn()`-style helpers already used in `src/components/ui/` (`clsx` + `tailwind-merge`).
- Animations: `motion` package is available; match existing usage before adding new deps — **do not add npm dependencies without asking**, the project pins exact versions.

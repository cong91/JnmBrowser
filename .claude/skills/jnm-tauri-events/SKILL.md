---
name: jnm-tauri-events
description: Correct Tauri event patterns in JnmBrowser — await listen() before triggering work, unlisten cleanup, event-driven hooks, and cancel-vs-failure in launch flows. Use when working with listen/emit, profile launch, or use-*-events hooks.
---

# Tauri events & launch flows

State sync is **event-driven**: backend emits, frontend `use-*-events` hooks listen and update React state. No manual refresh plumbing.

## The #1 gotcha: `listen()` is async

Always `await listen(...)` **before** launching a profile or starting work that emits the event. Fire-and-forget loses events:

```ts
// ❌ WRONG — can miss profile-running-changed
void listen("profile-running-changed", handler);
await invoke("launch_browser_profile", { id });

// ✅ CORRECT
const unlisten = await listen("profile-running-changed", handler);
await invoke("launch_browser_profile", { id });
```

## Hook pattern (mirror `src/hooks/use-profile-events.ts`)

```ts
useEffect(() => {
  let unlisten: (() => void) | undefined;
  const setup = async () => {
    await loadInitial(); // initial invoke() load first
    unlisten = await listen("profiles-changed", () => {
      void loadInitial();
    });
  };
  void setup();
  return () => {
    unlisten?.(); // always clean up
  };
}, [loadInitial]);
```

- Keep the unlisten fn in a local variable captured by the cleanup — required under React StrictMode double-mount.
- Do initial data load via `invoke()` inside the same effect, before/while subscribing.

## Existing event hooks (extend these, don't create parallel ones)

`use-profile-events`, `use-proxy-events`, `use-group-events`, `use-vpn-events`, `use-sync-session`, `use-registration-events`, `use-login-events`, `use-extension-events`, `use-recorder-session`, `use-browser-state`, `use-update-notifications`, `use-app-update-notifications`, `use-team-locks`.

Common event names: `profiles-changed`, `profile-running-changed` — grep the string in `src-tauri/` to find emit sites before adding a listener for a new one.

## Launch helpers: cancel ≠ failure

Helpers that can abort intentionally (e.g. the window-resize warning flow, `window-resize-warning-dialog.tsx`) must **return a boolean / distinguish cancel from failure**, so the UI does not toast `launchFailed` when the user simply cancelled. When adding a pre-launch confirmation step, thread that distinction through the helper's return value.

## Backend side

Emit with Tauri v2 `app.emit("event-name", payload)` (or `Emitter` trait on `AppHandle`/`Window`) right after the state change lands. If a command changes shared state, emitting the matching `*-changed` event is part of the command contract — see `jnm-tauri-command`.

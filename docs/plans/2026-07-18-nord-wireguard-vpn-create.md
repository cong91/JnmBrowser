# Nord / WireGuard VPN Create UX Implementation Plan

> **For the executing agent:** REQUIRED SUB-SKILL: Use skill({ name: "executing-plans" }) to implement this plan task-by-task.

**Goal:** Let users create a per-profile VPN from Proxies & VPNs by choosing a VPN source type, pasting the matching key/token, optionally picking a location/server, and saving a normal WireGuard config that reuses the existing tunnel path.

**Architecture:** Keep runtime VPN as `VpnType::WireGuard` only. Add a **create-source** UX (WireGuard manual vs Nord Access Token) that always ends in a WireGuard `.conf` stored via existing `create_vpn_config_manual` / storage. Nord path: Access Token → `nordlynx_private_key` + public server recommendations → build conf → save. No Nord CLI, no system-wide connect, no OpenVPN reintroduction in v1.

**Tech Stack:** Tauri v2 commands (Rust), existing `vpn` module + `vpn-worker` SOCKS path, React dialogs under `src/components/`, i18n 7 locales, `reqwest` for Nord HTTPS APIs.

---

## Must-Haves

**Goal:** User can open Proxies & VPNs → VPNs → Create, choose how to add a VPN, enter the right secret, optionally choose location, and get a reusable per-profile WireGuard entry.

### Observable Truths

1. User can open **Create** and see a clear **VPN source type** selector (at least: WireGuard manual, NordVPN Access Token).
2. Choosing a type shows **only the matching input fields** (not one overloaded form).
3. For **WireGuard manual**, user can paste private key + peer fields (current behavior, possibly cleaned up) and save.
4. For **NordVPN**, user can paste **Access Token**, optionally pick **country/location** (and optionally a specific server), and create one WireGuard config without leaving the app.
5. Created Nord configs appear in the same VPN list as imported WireGuard configs and can be assigned to a profile via existing `vpn_id`.
6. Launching a profile with that `vpn_id` still uses `start_vpn_worker` → local SOCKS (no Nord CLI, host IP unchanged).
7. Token/private key are not logged; storage remains encrypted like existing VPN config_data.
8. All new user-facing strings exist in **all 7** locale files (no `t(key, fallback)`).

### Required Artifacts

| Artifact | Provides | Path |
|----------|----------|------|
| Nord credentials client | Token → `nordlynx_private_key` (+ optional openvpn fields unused in v1) | `src-tauri/src/vpn/nord.rs` (new) |
| Nord server listing | Countries + WG recommendations | same module |
| Conf builder | Build standard WG conf text | same module |
| Tauri commands | FE invoke surface | `src-tauri/src/lib.rs` |
| Module exports | `pub mod` / re-exports | `src-tauri/src/vpn/mod.rs` |
| Create form UX | Type selector + conditional inserts | `src/components/vpn-form-dialog.tsx` |
| Types | FE types for servers / create payload | `src/types.ts` |
| i18n | Labels, errors, placeholders | `src/i18n/locales/{en,es,fr,ja,pt,ru,zh}.json` |
| Unit tests | Auth header shape, conf build, parse API JSON | `src-tauri/src/vpn/nord.rs` tests + existing vpn tests |

### Key Links

| From | To | Via | Risk |
|------|-----|-----|------|
| Create dialog Nord mode | Nord credentials API | `invoke("fetch_nord_wireguard_credentials")` | Token invalid / expired / wrong auth scheme |
| Create dialog | Recommendations API | `invoke("list_nord_wireguard_servers")` | Country id mismatch; empty list |
| Create dialog | Storage | `create_vpn_config_manual` or dedicated create command | Invalid conf fields fail parse |
| Profile launch | Tunnel | existing `start_vpn_worker` | Conf works parse-wise but handshake fails (Address/DNS/endpoint) |
| List UI | User mental model | badge still "WireGuard" | User expects type "Nord" in list — document as source-only, runtime WG |

### Out of scope (v1)

- OpenVPN import / service-credentials tunnel
- Nord CLI connect in Proxies & VPN
- Auto-reg integration with Nord WG pool (can be phase 2)
- Storing Access Token long-term as a first-class account (optional later; v1 can be paste-per-create)
- Multi-server batch generate in one click (nice-to-have; single create first)
- Router setup docs beyond short help text

### Proven API (from live PoC 2026-07-18)

```http
GET https://api.nordvpn.com/v1/users/services/credentials
Authorization: Basic base64("token:<ACCESS_TOKEN>")
```

Response includes:

- `nordlynx_private_key` (WireGuard private key)
- `username` / `password` (OpenVPN service credentials — ignore for v1 tunnel)

```http
GET https://api.nordvpn.com/v1/servers/countries
GET https://api.nordvpn.com/v1/servers/recommendations?limit=N&filters[servers_technologies][identifier]=wireguard_udp&filters[country_id]=<id>
```

Server object fields needed:

- `name`, `hostname`, `station`, `load`
- `technologies[]` where `identifier == "wireguard_udp"` → `metadata[name=public_key].value`

Default conf constants (NordLynx community standard):

```ini
Address = 10.5.0.2/32
DNS = 103.86.96.100
Endpoint = <station>:51820
AllowedIPs = 0.0.0.0/0
PersistentKeepalive = 25
```

---

## UX Spec (matches anh’s request)

### Entry

`Proxies & VPNs` → tab **VPNs** → **+ Create** (existing button).

### Step flow (single dialog)

```
[1] VPN source type
    ○ WireGuard (manual)
    ○ NordVPN (Access Token)

[2] Fields depend on type  ← "insert tương ứng"

    WireGuard:
      - Name
      - Private key
      - Address, DNS, MTU (optional advanced)
      - Peer public key, Endpoint, AllowedIPs, Keepalive, PSK

    NordVPN:
      - Name (optional; default from location/server)
      - Access Token (password input)
      - Location / Country (Select; optional "Best / Recommended")
      - Server (Select; optional auto-pick lowest load after country)
      - [Fetch servers] when country changes
      - Short help: where to get token (Nord account → manual access token)

[3] Create → save WireGuard VpnConfig → list refresh
```

### List display (v1)

- Keep `vpn_type: WireGuard` in storage/runtime.
- Optional display name prefix/suffix: e.g. `Nord · Japan #1259` so users recognize source.
- Do **not** add `VpnType::Nord` to tunnel enum in v1 (avoids dual backends).

### Import button

- Unchanged: drop WireGuard `.conf` only.
- Optional later: “I already have a generated conf” remains Import.

---

## Task Dependencies

```
Task 1 nord.rs client + conf builder + unit tests
Task 2 Tauri commands + register in lib.rs
Task 3 FE types
Task 4 vpn-form-dialog type selector + Nord fields
Task 5 i18n 7 locales
Task 6 wire list naming / empty copy if needed
Task 7 verify format/lint/unit tests
```

Wave 1: Task 1  
Wave 2: Task 2 (needs 1)  
Wave 3: Tasks 3–5 (after 2 for invoke names; 3–5 can parallelize carefully)  
Wave 4: Task 6–7

---

## Task 1: Rust Nord WireGuard helper module

**Files:**

- Create: `src-tauri/src/vpn/nord.rs`
- Modify: `src-tauri/src/vpn/mod.rs`

**Step 1: Add module skeleton + types**

```rust
// nord.rs
//! NordVPN helpers for building WireGuard (NordLynx) configs from an Access Token.
//! Runtime tunnel remains WireGuard via existing vpn-worker.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NordCredentials {
  pub nordlynx_private_key: String,
  // keep optional for future OpenVPN; do not return password to FE logs
  pub openvpn_username: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NordCountry {
  pub id: u32,
  pub name: String,
  pub code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NordWireGuardServer {
  pub name: String,
  pub hostname: String,
  pub station: String,
  pub load: u32,
  pub public_key: String,
  pub country_code: Option<String>,
  pub country_name: Option<String>,
}
```

**Step 2: Implement credentials fetch**

- URL: `https://api.nordvpn.com/v1/users/services/credentials`
- Basic auth username `"token"`, password = access_token.trim()
- Map JSON field `nordlynx_private_key`
- Error: empty token, HTTP non-200, missing key

**Step 3: Implement countries + recommendations**

- Countries: `https://api.nordvpn.com/v1/servers/countries`
- Recommendations: wireguard_udp filter + optional `filters[country_id]`
- Extract public_key from technologies metadata
- Skip servers missing pubkey

**Step 4: Conf builder**

```rust
pub fn build_nord_wireguard_conf(private_key: &str, server: &NordWireGuardServer) -> String {
  format!(
    "[Interface]\nPrivateKey = {pk}\nAddress = 10.5.0.2/32\nDNS = 103.86.96.100\n\n\
     [Peer]\nPublicKey = {pubk}\nEndpoint = {ip}:51820\nAllowedIPs = 0.0.0.0/0\nPersistentKeepalive = 25\n",
    pk = private_key.trim(),
    pubk = server.public_key.trim(),
    ip = server.station.trim(),
  )
}
```

Validate with existing `parse_wireguard_config` before save.

**Step 5: Unit tests**

- `basic_auth_token_user_scheme` (header construction / arg order)
- `build_conf_contains_required_sections`
- `parse_recommendation_json_extracts_pubkey` with fixture string
- Do **not** call live Nord APIs in unit tests

**Step 6: Export from `mod.rs`**

```rust
mod nord;
pub use nord::{...};
```

---

## Task 2: Tauri commands

**Files:**

- Modify: `src-tauri/src/lib.rs` (near existing VPN commands ~1080+)

**Commands (names exact):**

| Command | Args | Returns |
|---------|------|---------|
| `fetch_nord_wireguard_credentials` | `access_token: String` | `{ nordlynx_private_key: String }` only — **do not return openvpn password to FE unless needed** |
| `list_nord_countries` | none | `Vec<NordCountry>` |
| `list_nord_wireguard_servers` | `country_id: Option<u32>`, `limit: Option<u32>` | `Vec<NordWireGuardServer>` |
| `create_vpn_from_nord_token` | `access_token`, `country_id: Option<u32>`, `server_hostname: Option<String>`, `name: Option<String>` | `VpnConfig` |

**`create_vpn_from_nord_token` algorithm:**

1. Fetch credentials → private key  
2. If `server_hostname` set: find in recommendations (or fetch list and match); else pick first/lowest load for country (or global recommend if no country)  
3. `build_nord_wireguard_conf`  
4. `parse_wireguard_config` must Ok  
5. Default name: `name` or `Nord · {server.name}`  
6. `storage.create_config_manual(name, VpnType::WireGuard, conf)`  
7. Emit path: command returns config; FE emits `vpn-configs-changed` (same as form today)

Register all four in `invoke_handler`.

**Security:**

- Never `log::info!` token or private key  
- On error, return safe strings: `"Invalid or expired Nord access token"`, `"No WireGuard servers found"`, etc.

---

## Task 3: Frontend types

**Files:**

- Modify: `src/types.ts` (VPN section ~750+)

Add:

```ts
export type VpnCreateSource = "wireguard" | "nord";

export interface NordCountry {
  id: number;
  name: string;
  code: string;
}

export interface NordWireGuardServer {
  name: string;
  hostname: string;
  station: string;
  load: number;
  public_key: string;
  country_code?: string;
  country_name?: string;
}

export interface NordWireGuardCredentials {
  nordlynx_private_key: string;
}
```

Keep `VpnType = "WireGuard"` unchanged for stored configs.

---

## Task 4: Create dialog — type selector + conditional form

**Files:**

- Modify: `src/components/vpn-form-dialog.tsx`
- Possibly reuse UI primitives already used in proxy/registration dialogs (`Select`, `Label`, password `Input`)

**Behavior:**

1. On create (not edit), show source type control:
   - Radio group or Select: `wireguard` | `nord`
2. `wireguard` → existing WireGuard fields + existing `create_vpn_config_manual` submit path
3. `nord` → fields:
   - Name (optional)
   - Access Token (`type="password"`, paste-friendly)
   - Country select (load via `list_nord_countries` when switching to nord or on open)
   - Server select (load via `list_nord_wireguard_servers` when country changes; allow empty country = best recommended)
   - Loading states on country/server fetch
4. Submit Nord:
   - Prefer single invoke `create_vpn_from_nord_token` (server-side build) so private key never needs multi-step FE handling
   - Alternative acceptable: FE fetches key + servers then builds conf and calls `create_vpn_config_manual` — **prefer server-side create** to reduce key surface in FE state time
5. Edit mode: **name only** (current behavior); no source type switch when editing
6. Theme: semantic classes only (`primary`, `muted`, `destructive`, …) — no hardcoded colors
7. All copy via `t("vpns....")`

**Wire into management dialog:** no change required if still using `VpnFormDialog` for Create — verify `proxy-management-dialog.tsx` still opens it.

---

## Task 5: i18n (mandatory, all 7 locales)

**Files:**

- `src/i18n/locales/en.json` (and es, fr, ja, pt, ru, zh) under `vpns.form` / `vpns.nord`

Suggested keys (English first, then translate all):

```json
"vpns": {
  "form": {
    "titleCreate": "Create VPN",
    "descCreate": "Choose a VPN source and enter the matching details.",
    "sourceType": "VPN source",
    "sourceWireGuard": "WireGuard (manual)",
    "sourceNord": "NordVPN (Access Token)",
    "accessToken": "Nord access token",
    "accessTokenPlaceholder": "Paste token from Nord account",
    "accessTokenHelp": "NordAccount → NordVPN → generate a manual access token. Token is used once to create a WireGuard config.",
    "location": "Location",
    "locationAny": "Recommended (any)",
    "server": "Server",
    "serverAuto": "Best available",
    "loadingCountries": "Loading countries…",
    "loadingServers": "Loading servers…",
    "nordCreateSuccess": "NordVPN WireGuard config created",
    "nordTokenRequired": "Access token is required",
    "nordTokenInvalid": "Invalid or expired access token",
    "nordNoServers": "No WireGuard servers found for this location"
  }
}
```

Update old `titleCreate` / `descCreate` that currently say only WireGuard so Create is generic.

No empty strings in non-English locales.

---

## Task 6: List / empty state polish (small)

**Files:**

- `src/components/proxy-management-dialog.tsx` only if needed
- i18n empty copy already: “Import or create…”

Optional:

- When showing VPN row, if name starts with `Nord ·` no special case required
- Ensure Create button still opens form dialog (screenshot empty state)

No second Create entry point.

---

## Task 7: Verification

Run from repo root:

```bash
pnpm format
pnpm lint
pnpm test:rust:unit
```

Minimum green:

- New nord unit tests
- Existing vpn parse/storage tests
- `pnpm check-unused-commands` if part of lint/test pipeline — new commands must be referenced from FE

**Manual checklist (anh):**

1. Create → WireGuard manual still works  
2. Create → Nord → paste token → pick Japan → Create  
3. VPN list shows new entry  
4. Assign to profile → launch → profile IP is Nord location  
5. Host IP unchanged  
6. Invalid token → toast error, no partial save  

---

## Implementation notes / decisions locked

| Decision | Choice |
|----------|--------|
| Runtime VPN type | WireGuard only |
| Nord secret input | Access Token (not OpenVPN service password, not NordLynx key paste-only — token is primary; optional advanced “paste private key only” can wait) |
| Location | Optional country + optional server |
| Dynamic IP later | Same private key, new create or future “rotate peer” — not v1 |
| OpenVPN | Out of v1 |
| Token storage | v1: paste per create; do not persist token unless product asks later |
| Auth scheme | Basic `token:<access_token>` only (PoC proven) |

---

## Suggested commit slices

1. `feat(vpn): add Nord token credentials and WG conf builder`  
2. `feat(vpn): tauri commands for Nord WireGuard create`  
3. `feat(ui): VPN create source selector and Nord location form`  
4. `feat(i18n): Nord VPN create strings for all locales`  

---

## Done criteria

- [ ] User can choose VPN source type in Create dialog  
- [ ] Matching inputs shown per type  
- [ ] Nord path: token + optional location/server → WireGuard config in list  
- [ ] Per-profile assignment/launch still uses existing worker (no system Nord)  
- [ ] Tests + lint/format green  
- [ ] No secrets in logs or git  
- [ ] 7 locales complete  

---

## Execution handoff

Plan saved to `docs/plans/2026-07-18-nord-wireguard-vpn-create.md`.

**Two execution options:**

1. **Subagent-Driven (this session)** — implement task-by-task here with review between tasks  
2. **Parallel / new session** — open a session with `executing-plans` and run the plan file  

Which approach do you want?

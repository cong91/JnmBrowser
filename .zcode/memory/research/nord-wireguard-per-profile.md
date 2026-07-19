_Written: 2026-07-18T06:28:03.988Z_

# Nord → WireGuard per-profile (implemented 2026-07-18)

## Implemented
- `src-tauri/src/vpn/nord.rs`: token Basic auth `token:<access_token>` → nordlynx_private_key; countries; WG recommendations; conf builder
- Tauri: `fetch_nord_wireguard_credentials`, `list_nord_countries`, `list_nord_wireguard_servers`, `create_vpn_from_nord_token`
- UI Create dialog: source WireGuard | Nord, location + server selects
- Runtime type remains WireGuard; name default `Nord · {server}`
- i18n en/es/fr/ja/pt/ru/zh
- Tests: 6 nord unit tests pass; unused-commands pass; clippy lib green after sort_by_key fixes

## User flow
Proxies & VPNs → VPNs → Create → NordVPN (Access Token) → paste token → optional country/server → Create

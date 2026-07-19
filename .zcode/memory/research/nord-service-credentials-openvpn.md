_Written: 2026-07-18T06:06:59.698Z_

# Nord Service Credentials → per-profile research (2026-07-18)

## User hypothesis
Service credentials + OpenVPN config files enable manual per-profile VPN.

## Verdict
Hypothesis is correct for Nord's official manual model (OpenVPN).
It is NOT usable in JnmBrowser today — OpenVPN was removed; only WireGuard remains.

## Nord official pieces
- Service credentials username/password (NOT account email)
- OpenVPN .ovpn UDP/TCP downloads per hostname
- Same credentials work across many .ovpn server files
- Gluetun/community: OPENVPN_USER + OPENVPN_PASSWORD + server filter

## App reality
- VpnType = WireGuard only
- detect rejects .ovpn
- storage drops legacy OpenVPN JSON
- per-profile path: vpn_id → vpn-worker → SOCKS5 loopback → browser only

## Path to product
Re-add OpenVPN type OR external openvpn process exposing SOCKS/HTTP local proxy per profile.
WireGuard NordLynx export still unavailable on user's portal.

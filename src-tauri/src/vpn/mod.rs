//! VPN support module for WireGuard configurations.
//!
//! This module provides:
//! - WireGuard config parsing (`.conf` files)
//! - Encrypted storage for VPN configurations
//! - Tunnel management with userspace WireGuard (boringtun) routed through smoltcp
//! - NordVPN access-token helpers that produce WireGuard configs

mod config;
pub mod nord;
pub mod socks5_server;
mod storage;
mod tunnel;
mod wireguard;

pub use config::{
  detect_vpn_type, parse_wireguard_config, VpnConfig, VpnError, VpnImportResult, VpnStatus,
  VpnType, WireGuardConfig,
};
pub use nord::{
  build_nord_wireguard_conf, build_rotated_nord_wireguard_conf, default_nord_vpn_name,
  detect_nord_max_sessions, extract_wireguard_peer_endpoint_host,
  extract_wireguard_peer_public_key, extract_wireguard_private_key,
  fetch_nord_wireguard_credentials, list_nord_countries, list_nord_wireguard_servers,
  pick_distinct_nord_servers, pick_nord_server, pick_rotated_nord_server,
  spawn_ephemeral_nord_peer_pool, validate_nord_wireguard_conf, NordCountry,
  NordWireGuardCredentials, NordWireGuardServer,
};
pub use storage::VpnStorage;
pub use tunnel::{TunnelManager, VpnTunnel};
pub use wireguard::WireGuardTunnel;

use once_cell::sync::Lazy;
use std::sync::Mutex;

/// Global VPN storage instance
pub static VPN_STORAGE: Lazy<Mutex<VpnStorage>> = Lazy::new(|| Mutex::new(VpnStorage::new()));

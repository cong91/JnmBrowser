//! NordVPN helpers for building WireGuard (NordLynx) configs from an Access Token.
//!
//! Runtime tunnels remain WireGuard via the existing vpn-worker path. This module
//! only fetches credentials / server metadata and builds a standard `.conf`.

use super::config::{parse_wireguard_config, VpnError};
use serde::{Deserialize, Serialize};

const NORD_API_BASE: &str = "https://api.nordvpn.com/v1";
const NORD_WG_ADDRESS: &str = "10.5.0.2/32";
const NORD_WG_DNS: &str = "103.86.96.100";
const NORD_WG_PORT: u16 = 51820;
const NORD_WG_KEEPALIVE: u16 = 25;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NordWireGuardCredentials {
  pub nordlynx_private_key: String,
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

#[derive(Debug, Deserialize)]
struct NordCredentialsResponse {
  nordlynx_private_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NordCountryResponse {
  id: u32,
  name: String,
  code: String,
}

#[derive(Debug, Deserialize)]
struct NordServerResponse {
  name: Option<String>,
  hostname: Option<String>,
  station: Option<String>,
  load: Option<u32>,
  technologies: Option<Vec<NordTechnology>>,
  locations: Option<Vec<NordLocation>>,
}

#[derive(Debug, Deserialize)]
struct NordTechnology {
  identifier: Option<String>,
  metadata: Option<Vec<NordMetadata>>,
}

#[derive(Debug, Deserialize)]
struct NordMetadata {
  name: Option<String>,
  value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NordLocation {
  country: Option<NordLocationCountry>,
}

#[derive(Debug, Deserialize)]
struct NordLocationCountry {
  name: Option<String>,
  code: Option<String>,
}

/// Build HTTP Basic auth username/password for Nord access tokens.
/// Username must be the literal `token`; password is the access token.
pub fn nord_token_basic_auth(access_token: &str) -> Result<(&str, &str), String> {
  let token = access_token.trim();
  if token.is_empty() {
    return Err("Access token is required".to_string());
  }
  Ok(("token", token))
}

/// Build a WireGuard conf for a NordLynx private key + recommended server.
pub fn build_nord_wireguard_conf(private_key: &str, server: &NordWireGuardServer) -> String {
  let pk = private_key.trim();
  let pubk = server.public_key.trim();
  let ip = server.station.trim();
  format!(
    "[Interface]\n\
PrivateKey = {pk}\n\
Address = {NORD_WG_ADDRESS}\n\
DNS = {NORD_WG_DNS}\n\
\n\
[Peer]\n\
PublicKey = {pubk}\n\
Endpoint = {ip}:{NORD_WG_PORT}\n\
AllowedIPs = 0.0.0.0/0\n\
PersistentKeepalive = {NORD_WG_KEEPALIVE}\n"
  )
}

/// Default display name for a Nord-created WireGuard config.
pub fn default_nord_vpn_name(server: &NordWireGuardServer) -> String {
  format!("Nord · {}", server.name)
}

fn wireguard_public_key(technologies: &[NordTechnology]) -> Option<String> {
  for tech in technologies {
    if tech.identifier.as_deref() != Some("wireguard_udp") {
      continue;
    }
    for meta in tech.metadata.as_deref().unwrap_or(&[]) {
      if meta.name.as_deref() == Some("public_key") {
        if let Some(value) = meta
          .value
          .as_ref()
          .map(|s| s.trim())
          .filter(|s| !s.is_empty())
        {
          return Some(value.to_string());
        }
      }
    }
  }
  None
}

fn map_server(raw: NordServerResponse) -> Option<NordWireGuardServer> {
  let hostname = raw.hostname?.trim().to_string();
  if hostname.is_empty() {
    return None;
  }
  let station = raw.station.unwrap_or_default().trim().to_string();
  if station.is_empty() {
    return None;
  }
  let public_key = wireguard_public_key(raw.technologies.as_deref().unwrap_or(&[]))?;
  let name = raw
    .name
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .unwrap_or_else(|| hostname.clone());
  let (country_name, country_code) = raw
    .locations
    .as_ref()
    .and_then(|locs| locs.first())
    .and_then(|loc| loc.country.as_ref())
    .map(|c| (c.name.clone(), c.code.clone()))
    .unwrap_or((None, None));

  Some(NordWireGuardServer {
    name,
    hostname,
    station,
    load: raw.load.unwrap_or(0),
    public_key,
    country_code,
    country_name,
  })
}

fn http_client() -> Result<reqwest::Client, String> {
  reqwest::Client::builder()
    .timeout(std::time::Duration::from_secs(30))
    .build()
    .map_err(|e| format!("Failed to create HTTP client: {e}"))
}

/// Fetch NordLynx WireGuard private key using a manual access token.
pub async fn fetch_nord_wireguard_credentials(
  access_token: &str,
) -> Result<NordWireGuardCredentials, String> {
  let (user, pass) = nord_token_basic_auth(access_token)?;
  let client = http_client()?;
  let response = client
    .get(format!("{NORD_API_BASE}/users/services/credentials"))
    .basic_auth(user, Some(pass))
    .send()
    .await
    .map_err(|e| format!("Failed to reach NordVPN API: {e}"))?;

  let status = response.status();
  if !status.is_success() {
    if status.as_u16() == 401 || status.as_u16() == 403 {
      return Err("Invalid or expired Nord access token".to_string());
    }
    return Err(format!(
      "NordVPN credentials request failed (HTTP {})",
      status.as_u16()
    ));
  }

  let body: NordCredentialsResponse = response
    .json()
    .await
    .map_err(|_| "Invalid or expired Nord access token".to_string())?;

  let key = body
    .nordlynx_private_key
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .ok_or_else(|| "Nord account did not return a WireGuard private key".to_string())?;

  Ok(NordWireGuardCredentials {
    nordlynx_private_key: key,
  })
}

/// List Nord countries for the location picker.
pub async fn list_nord_countries() -> Result<Vec<NordCountry>, String> {
  let client = http_client()?;
  let response = client
    .get(format!("{NORD_API_BASE}/servers/countries"))
    .send()
    .await
    .map_err(|e| format!("Failed to reach NordVPN API: {e}"))?;

  if !response.status().is_success() {
    return Err(format!(
      "NordVPN countries request failed (HTTP {})",
      response.status().as_u16()
    ));
  }

  let raw: Vec<NordCountryResponse> = response
    .json()
    .await
    .map_err(|e| format!("Failed to parse NordVPN countries: {e}"))?;

  let mut countries: Vec<NordCountry> = raw
    .into_iter()
    .filter(|c| !c.name.trim().is_empty() && !c.code.trim().is_empty())
    .map(|c| NordCountry {
      id: c.id,
      name: c.name,
      code: c.code,
    })
    .collect();
  countries.sort_by_key(|a| a.name.to_lowercase());
  Ok(countries)
}

/// List recommended WireGuard servers, optionally filtered by country.
pub async fn list_nord_wireguard_servers(
  country_id: Option<u32>,
  limit: Option<u32>,
) -> Result<Vec<NordWireGuardServer>, String> {
  let limit = limit.unwrap_or(20).clamp(1, 100);
  let mut url = format!(
    "{NORD_API_BASE}/servers/recommendations?limit={limit}&filters%5Bservers_technologies%5D%5Bidentifier%5D=wireguard_udp"
  );
  if let Some(id) = country_id {
    url.push_str(&format!("&filters%5Bcountry_id%5D={id}"));
  }

  let client = http_client()?;
  let response = client
    .get(url)
    .send()
    .await
    .map_err(|e| format!("Failed to reach NordVPN API: {e}"))?;

  if !response.status().is_success() {
    return Err(format!(
      "NordVPN servers request failed (HTTP {})",
      response.status().as_u16()
    ));
  }

  let raw: Vec<NordServerResponse> = response
    .json()
    .await
    .map_err(|e| format!("Failed to parse NordVPN servers: {e}"))?;

  let mut servers: Vec<NordWireGuardServer> = raw.into_iter().filter_map(map_server).collect();
  servers.sort_by(|a, b| a.load.cmp(&b.load).then_with(|| a.name.cmp(&b.name)));
  Ok(servers)
}

/// Pick a server from a list by hostname, or the lowest-load entry.
pub fn pick_nord_server<'a>(
  servers: &'a [NordWireGuardServer],
  server_hostname: Option<&str>,
) -> Result<&'a NordWireGuardServer, String> {
  if servers.is_empty() {
    return Err("No WireGuard servers found for this location".to_string());
  }
  if let Some(hostname) = server_hostname.map(str::trim).filter(|s| !s.is_empty()) {
    return servers
      .iter()
      .find(|s| s.hostname.eq_ignore_ascii_case(hostname))
      .ok_or_else(|| format!("WireGuard server not found: {hostname}"));
  }
  Ok(&servers[0])
}

/// Extract Interface PrivateKey from a WireGuard conf.
pub fn extract_wireguard_private_key(conf: &str) -> Result<String, String> {
  let parsed = parse_wireguard_config(conf).map_err(|e| e.to_string())?;
  let key = parsed.private_key.trim();
  if key.is_empty() {
    return Err("WireGuard config missing PrivateKey".into());
  }
  Ok(key.to_string())
}

/// Peer endpoint host/IP without port (for avoid-list during rotate).
pub fn extract_wireguard_peer_endpoint_host(conf: &str) -> Option<String> {
  let parsed = parse_wireguard_config(conf).ok()?;
  let endpoint = parsed.peer_endpoint.trim();
  let host = endpoint
    .rsplit_once(':')
    .map(|(h, _)| h)
    .unwrap_or(endpoint);
  let host = host.trim().trim_matches(|c| c == '[' || c == ']');
  if host.is_empty() {
    None
  } else {
    Some(host.to_string())
  }
}

/// Peer public key from conf (avoid same peer on rotate).
pub fn extract_wireguard_peer_public_key(conf: &str) -> Option<String> {
  let parsed = parse_wireguard_config(conf).ok()?;
  let key = parsed.peer_public_key.trim();
  if key.is_empty() {
    None
  } else {
    Some(key.to_string())
  }
}

/// Prefer a different server than the current station/public key (lowest load first).
pub fn pick_rotated_nord_server<'a>(
  servers: &'a [NordWireGuardServer],
  avoid_station: Option<&str>,
  avoid_public_key: Option<&str>,
) -> Result<&'a NordWireGuardServer, String> {
  if servers.is_empty() {
    return Err("No WireGuard servers found for rotation".to_string());
  }
  let avoid_station = avoid_station.map(str::trim).filter(|s| !s.is_empty());
  let avoid_pk = avoid_public_key.map(str::trim).filter(|s| !s.is_empty());

  let different = servers.iter().find(|s| {
    let station_ok = avoid_station.is_none_or(|a| !s.station.eq_ignore_ascii_case(a));
    let pk_ok = avoid_pk.is_none_or(|a| s.public_key.trim() != a);
    station_ok && pk_ok
  });
  if let Some(s) = different {
    return Ok(s);
  }
  // All candidates match avoid list — still return best available so batch can continue.
  Ok(&servers[0])
}

/// Fetch a new Nord peer and build conf, avoiding the current endpoint when possible.
pub async fn build_rotated_nord_wireguard_conf(
  private_key: &str,
  avoid_station: Option<&str>,
  avoid_public_key: Option<&str>,
  country_id: Option<u32>,
) -> Result<(NordWireGuardServer, String), String> {
  let servers = list_nord_wireguard_servers(country_id, Some(40)).await?;
  let server = pick_rotated_nord_server(&servers, avoid_station, avoid_public_key)?.clone();
  let conf = build_nord_wireguard_conf(private_key, &server);
  validate_nord_wireguard_conf(&conf).map_err(|e| e.to_string())?;
  Ok((server, conf))
}

/// Pick `count` servers preferring distinct stations (lowest load first).
/// If fewer unique stations exist, cycles unique peers so the pool size is `count`.
pub fn pick_distinct_nord_servers(
  servers: &[NordWireGuardServer],
  count: usize,
) -> Result<Vec<NordWireGuardServer>, String> {
  if servers.is_empty() {
    return Err("No WireGuard servers available for peer pool".into());
  }
  let count = count.clamp(1, 6);
  let mut unique: Vec<NordWireGuardServer> = Vec::new();
  let mut seen_stations = std::collections::HashSet::new();
  for s in servers {
    let station = s.station.trim();
    if station.is_empty() || !seen_stations.insert(station.to_string()) {
      continue;
    }
    unique.push(s.clone());
  }
  if unique.is_empty() {
    return Err("No distinct WireGuard servers for peer pool".into());
  }
  let mut out = Vec::with_capacity(count);
  for i in 0..count {
    out.push(unique[i % unique.len()].clone());
  }
  Ok(out)
}

/// Spawn `count` ephemeral WireGuard inventory configs (same private key, distinct peers).
/// Caller must delete returned ids after the batch.
pub async fn spawn_ephemeral_nord_peer_pool(
  private_key: &str,
  count: usize,
  country_id: Option<u32>,
  name_prefix: &str,
) -> Result<Vec<String>, String> {
  let count = count.clamp(1, 6);
  let limit = (count as u32).saturating_mul(4).clamp(10, 80);
  let servers = list_nord_wireguard_servers(country_id, Some(limit)).await?;
  let peers = pick_distinct_nord_servers(&servers, count)?;

  let storage = crate::vpn::VPN_STORAGE
    .lock()
    .map_err(|e| format!("Failed to lock VPN storage: {e}"))?;

  let mut ids = Vec::with_capacity(peers.len());
  for (i, server) in peers.iter().enumerate() {
    let conf = build_nord_wireguard_conf(private_key, server);
    validate_nord_wireguard_conf(&conf).map_err(|e| e.to_string())?;
    let name = format!("{name_prefix} · {} · s{i}", server.name);
    let created = storage
      .create_ephemeral_wireguard(&name, &conf)
      .map_err(|e| format!("Create ephemeral VPN conf: {e}"))?;
    ids.push(created.id);
  }
  Ok(ids)
}

/// Validate that a built Nord conf is accepted by the WireGuard parser.
pub fn validate_nord_wireguard_conf(conf: &str) -> Result<(), VpnError> {
  parse_wireguard_config(conf).map(|_| ())
}

/// Nord simultaneous-session budget for WireGuard auto-reg.
///
/// Product policy: **always 6** (clean number under the ~10-device Nord plan ceiling).
/// API probe is best-effort only; if a smaller limit is ever advertised, we respect
/// it, but we never go above 6.
pub async fn detect_nord_max_sessions(access_token: &str) -> u32 {
  const POLICY_MAX: u32 = 6;
  match fetch_nord_service_session_limit(access_token).await {
    Ok(Some(n)) if n >= 1 => n.min(POLICY_MAX),
    _ => POLICY_MAX,
  }
}

async fn fetch_nord_service_session_limit(access_token: &str) -> Result<Option<u32>, String> {
  let (user, pass) = nord_token_basic_auth(access_token)?;
  let client = http_client()?;
  // Community-known endpoint; field names vary by plan — parse heuristically.
  let response = client
    .get(format!("{NORD_API_BASE}/users/services"))
    .basic_auth(user, Some(pass))
    .send()
    .await
    .map_err(|e| format!("Failed to reach NordVPN services API: {e}"))?;
  if !response.status().is_success() {
    return Ok(None);
  }
  let value: serde_json::Value = response
    .json()
    .await
    .map_err(|e| format!("Failed to parse Nord services: {e}"))?;
  Ok(extract_session_limit_from_json(&value))
}

/// Heuristic extraction of simultaneous connection limits from Nord JSON blobs.
pub fn extract_session_limit_from_json(value: &serde_json::Value) -> Option<u32> {
  // Walk object/array looking for known keys.
  let keys = [
    "max_devices",
    "maxDevices",
    "devices_limit",
    "devicesLimit",
    "simultaneous_connections",
    "simultaneousConnections",
    "connection_limit",
    "connectionLimit",
    "max_connections",
    "maxConnections",
    "dedicated_ip",
  ];
  fn walk(v: &serde_json::Value, keys: &[&str]) -> Option<u32> {
    match v {
      serde_json::Value::Object(map) => {
        for k in keys {
          if let Some(val) = map.get(*k) {
            if let Some(n) = val.as_u64() {
              if n >= 1 {
                return Some(n as u32);
              }
            }
            if let Some(s) = val.as_str() {
              if let Ok(n) = s.parse::<u32>() {
                if n >= 1 {
                  return Some(n);
                }
              }
            }
          }
        }
        for (_k, child) in map {
          if let Some(n) = walk(child, keys) {
            return Some(n);
          }
        }
        None
      }
      serde_json::Value::Array(arr) => {
        for child in arr {
          if let Some(n) = walk(child, keys) {
            return Some(n);
          }
        }
        None
      }
      _ => None,
    }
  }
  walk(value, &keys)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn nord_token_basic_auth_rejects_empty() {
    assert!(nord_token_basic_auth("").is_err());
    assert!(nord_token_basic_auth("   ").is_err());
  }

  #[test]
  fn nord_token_basic_auth_uses_token_username() {
    let (user, pass) = nord_token_basic_auth("  abc123  ").unwrap();
    assert_eq!(user, "token");
    assert_eq!(pass, "abc123");
  }

  #[test]
  fn build_conf_contains_required_sections() {
    let server = NordWireGuardServer {
      name: "Japan #1".into(),
      hostname: "jp1.nordvpn.com".into(),
      station: "1.2.3.4".into(),
      load: 10,
      public_key: "SAio0Z0suFlRfmydzPdcn6MamqS7Mq4pSOm2YmJkLSs=".into(),
      country_code: Some("JP".into()),
      country_name: Some("Japan".into()),
    };
    // Valid 32-byte base64 private key (same style as fixtures)
    let private_key = "YEocP0e2o1WT5GlvBvQzVF7EeR6z9aCk+ZdZ5NKEuXA=";
    let conf = build_nord_wireguard_conf(private_key, &server);
    assert!(conf.contains("[Interface]"));
    assert!(conf.contains("[Peer]"));
    assert!(conf.contains("PrivateKey = YEocP0e2o1WT5GlvBvQzVF7EeR6z9aCk+ZdZ5NKEuXA="));
    assert!(conf.contains("Address = 10.5.0.2/32"));
    assert!(conf.contains("DNS = 103.86.96.100"));
    assert!(conf.contains("Endpoint = 1.2.3.4:51820"));
    assert!(conf.contains("PersistentKeepalive = 25"));
    validate_nord_wireguard_conf(&conf).expect("conf should parse");
  }

  #[test]
  fn default_name_uses_server_label() {
    let server = NordWireGuardServer {
      name: "Japan #1259".into(),
      hostname: "jp1259.nordvpn.com".into(),
      station: "1.1.1.1".into(),
      load: 1,
      public_key: "x".into(),
      country_code: None,
      country_name: None,
    };
    assert_eq!(default_nord_vpn_name(&server), "Nord · Japan #1259");
  }

  #[test]
  fn parse_recommendation_json_extracts_pubkey() {
    let raw = r#"{
      "name": "Vietnam #42",
      "hostname": "vn42.nordvpn.com",
      "station": "45.80.186.1",
      "load": 34,
      "technologies": [
        {
          "identifier": "wireguard_udp",
          "metadata": [{"name": "public_key", "value": "7tlYA3PdA5or5iw3VFJOwZrvhdT4FNSmXRk7SFd3/Bo="}]
        }
      ],
      "locations": [{"country": {"name": "Vietnam", "code": "VN"}}]
    }"#;
    let parsed: NordServerResponse = serde_json::from_str(raw).unwrap();
    let server = map_server(parsed).expect("mapped server");
    assert_eq!(server.hostname, "vn42.nordvpn.com");
    assert_eq!(server.station, "45.80.186.1");
    assert_eq!(
      server.public_key,
      "7tlYA3PdA5or5iw3VFJOwZrvhdT4FNSmXRk7SFd3/Bo="
    );
    assert_eq!(server.country_code.as_deref(), Some("VN"));
  }

  #[test]
  fn pick_server_by_hostname_or_lowest_load() {
    let servers = vec![
      NordWireGuardServer {
        name: "A".into(),
        hostname: "a.nordvpn.com".into(),
        station: "1.1.1.1".into(),
        load: 5,
        public_key: "k".into(),
        country_code: None,
        country_name: None,
      },
      NordWireGuardServer {
        name: "B".into(),
        hostname: "b.nordvpn.com".into(),
        station: "2.2.2.2".into(),
        load: 20,
        public_key: "k".into(),
        country_code: None,
        country_name: None,
      },
    ];
    assert_eq!(
      pick_nord_server(&servers, None).unwrap().hostname,
      "a.nordvpn.com"
    );
    assert_eq!(
      pick_nord_server(&servers, Some("b.nordvpn.com"))
        .unwrap()
        .hostname,
      "b.nordvpn.com"
    );
    assert!(pick_nord_server(&servers, Some("missing")).is_err());
    assert!(pick_nord_server(&[], None).is_err());
  }
  #[test]
  fn pick_rotated_prefers_different_station() {
    let servers = vec![
      NordWireGuardServer {
        name: "A".into(),
        hostname: "a.nordvpn.com".into(),
        station: "1.1.1.1".into(),
        load: 1,
        public_key: "pkA".into(),
        country_code: None,
        country_name: None,
      },
      NordWireGuardServer {
        name: "B".into(),
        hostname: "b.nordvpn.com".into(),
        station: "2.2.2.2".into(),
        load: 2,
        public_key: "pkB".into(),
        country_code: None,
        country_name: None,
      },
    ];
    let s = pick_rotated_nord_server(&servers, Some("1.1.1.1"), Some("pkA")).unwrap();
    assert_eq!(s.hostname, "b.nordvpn.com");
  }

  #[test]
  fn extract_private_key_from_conf() {
    let conf = "[Interface]
PrivateKey = YEocP0e2o1WT5GlvBvQzVF7EeR6z9aCk+ZdZ5NKEuXA=
Address = 10.5.0.2/32

[Peer]
PublicKey = SAio0Z0suFlRfmydzPdcn6MamqS7Mq4pSOm2YmJkLSs=
Endpoint = 1.2.3.4:51820
AllowedIPs = 0.0.0.0/0
";
    let conf = conf.to_string();
    assert_eq!(
      extract_wireguard_private_key(&conf).unwrap(),
      "YEocP0e2o1WT5GlvBvQzVF7EeR6z9aCk+ZdZ5NKEuXA="
    );
    assert_eq!(
      extract_wireguard_peer_endpoint_host(&conf).as_deref(),
      Some("1.2.3.4")
    );
  }

  #[test]
  fn pick_distinct_cycles_when_needed() {
    let servers = vec![
      NordWireGuardServer {
        name: "A".into(),
        hostname: "a.nordvpn.com".into(),
        station: "1.1.1.1".into(),
        load: 1,
        public_key: "pkA".into(),
        country_code: None,
        country_name: None,
      },
      NordWireGuardServer {
        name: "B".into(),
        hostname: "b.nordvpn.com".into(),
        station: "2.2.2.2".into(),
        load: 2,
        public_key: "pkB".into(),
        country_code: None,
        country_name: None,
      },
    ];
    let picked = pick_distinct_nord_servers(&servers, 4).unwrap();
    assert_eq!(picked.len(), 4);
    assert_eq!(picked[0].station, "1.1.1.1");
    assert_eq!(picked[1].station, "2.2.2.2");
    assert_eq!(picked[2].station, "1.1.1.1");
  }

  #[test]
  fn extract_session_limit_from_nested_json() {
    let v = serde_json::json!({
      "services": [{"details": {"max_devices": 6}}]
    });
    assert_eq!(extract_session_limit_from_json(&v), Some(6));
    let v2 = serde_json::json!({"foo": 1});
    assert_eq!(extract_session_limit_from_json(&v2), None);
  }
}

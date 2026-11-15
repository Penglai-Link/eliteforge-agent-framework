//! Product-neutral wire protocol for PersonAI remote access.
//!
//! This crate intentionally has no HTTP, WebSocket or Noise dependency. It
//! defines the identity, pairing, settings and frame vocabulary shared by the
//! server, the product packs and the mobile implementation.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

/// Single protocol name. There is no version suffix in this development phase.
pub const PROTOCOL: &str = "tron-remote";
/// Fixed Noise cipher suite.
pub const NOISE_SUITE: &str = "Noise_IK_25519_ChaChaPoly_SHA256";
/// Secure WebSocket endpoint on the remote listener.
pub const SECURE_PATH: &str = "/remote/secure";
/// Identity endpoint on the remote listener.
pub const IDENTITY_PATH: &str = "/remote/identity";
/// Tick used by the remote listener.
pub const TICKET_TTL_SECONDS_DEFAULT: u64 = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportMode {
    Public,
    Lan,
}

impl TransportMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Lan => "lan",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductIdentity {
    pub product_code: String,
    pub product_name: String,
    pub node_id: String,
    pub version: String,
    pub platform: String,
    pub machine_name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeIdentity {
    pub node_id: String,
    /// Gap25419 public key, base64url without padding.
    pub public_key: String,
    /// Gap25419 private key, base64url without padding.
    pub private_key: String,
    /// `sha256:<first 16 hex chars>`.
    pub key_id: String,
    pub created_at_ms: u64,
}

impl NodeIdentity {
    pub fn public_key_bytes(&self) -> Result<Vec<u8>, ProtocolError> {
        URL_SAFE_NO_PAD
            .decode(&self.public_key)
            .map_err(|_| ProtocolError::InvalidKey)
    }

    pub fn private_key_bytes(&self) -> Result<Vec<u8>, ProtocolError> {
        URL_SAFE_NO_PAD
            .decode(&self.private_key)
            .map_err(|_| ProtocolError::InvalidKey)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceRecord {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    /// Gap25419 public key, base64url without padding.
    pub public_key: String,
    pub key_id: String,
    pub created_at_ms: u64,
    pub last_seen_at_ms: u64,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub revoked: bool,
}

impl DeviceRecord {
    pub fn summary(&self) -> DeviceSummary {
        DeviceSummary {
            device_id: self.device_id.clone(),
            name: self.name.clone(),
            platform: self.platform.clone(),
            created_at_ms: self.created_at_ms,
            last_seen_at_ms: self.last_seen_at_ms,
            scopes: self.scopes.clone(),
            revoked: self.revoked,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceSummary {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub created_at_ms: u64,
    pub last_seen_at_ms: u64,
    pub scopes: Vec<String>,
    pub revoked: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSettings {
    #[serde(default)]
    pub enabled: bool,
    /// `https://...` enables the public profile; empty enables the LAN profile.
    #[serde(default)]
    pub public_base_url: String,
    /// Empty means loopback for public, all interfaces for LAN.
    #[serde(default)]
    pub bind_address: String,
    /// Optional LAN base URL used by the QR code when no public URL is set.
    #[serde(default)]
    pub lan_base_url: String,
    /// 0 means "main port + 1"; the service itself does not bind a port.
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    #[serde(default = "default_ticket_ttl")]
    pub ticket_ttl_seconds: u64,
    #[serde(default)]
    pub machine_name: String,
    #[serde(default)]
    pub product_code: String,
    #[serde(default)]
    pub product_name: String,
}

fn default_ticket_ttl() -> u64 {
    TICKET_TTL_SECONDS_DEFAULT
}

impl Default for RemoteSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            public_base_url: String::new(),
            bind_address: String::new(),
            lan_base_url: String::new(),
            port: 0,
            trusted_proxies: vec!["127.0.0.1/32".into(), "::1/128".into()],
            allowed_origins: Vec::new(),
            ticket_ttl_seconds: TICKET_TTL_SECONDS_DEFAULT,
            machine_name: String::new(),
            product_code: String::new(),
            product_name: String::new(),
        }
    }
}

impl RemoteSettings {
    pub fn is_public(&self) -> bool {
        self.public_base_url.trim().starts_with("https://")
    }

    pub fn transport(&self) -> TransportMode {
        if self.is_public() {
            TransportMode::Public
        } else {
            TransportMode::Lan
        }
    }

    pub fn ticket_ttl(&self) -> u64 {
        self.ticket_ttl_seconds.max(30)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthDevice {
    pub name: String,
    pub platform: String,
    #[serde(default)]
    pub app_version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadyFrame {
    pub device_id: String,
    pub session_id: String,
    pub expires_in: u64,
    pub methods: Vec<String>,
    pub product: ProductIdentity,
    pub events: String,
    pub features: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireFrame {
    Auth {
        #[serde(default)]
        ticket: Option<String>,
        #[serde(default)]
        device_id: Option<String>,
        #[serde(default)]
        device: Option<AuthDevice>,
        #[serde(default)]
        features: Option<Vec<String>>,
    },
    Ready(ReadyFrame),
    Rpc {
        id: u64,
        method: String,
        #[serde(default)]
        params: Value,
    },
    RpcResult {
        id: u64,
        result: Value,
    },
    RpcError {
        id: u64,
        code: String,
        message: String,
        #[serde(default)]
        details: Option<Value>,
    },
    RpcEvent {
        id: u64,
        data: Value,
        #[serde(default)]
        seq: Option<u64>,
    },
    RpcEnd {
        id: u64,
    },
    Cancel {
        id: u64,
    },
    Ping {
        ts: u64,
    },
    Pong {
        ts: u64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecureTransportAdvert {
    pub cipher: String,
    pub endpoint: String,
    pub key_id: String,
    pub public_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteIdentity {
    pub protocol: String,
    pub product_code: String,
    pub product_name: String,
    pub node_id: String,
    pub machine_name: String,
    pub version: String,
    pub platform: String,
    pub base_url: String,
    #[serde(default)]
    pub lan_base_url: String,
    #[serde(default)]
    pub public_base_url: String,
    pub secure_transport: SecureTransportAdvert,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingView {
    pub uri: String,
    pub expires_at_ms: u64,
    pub transport: TransportMode,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteStatus {
    pub enabled: bool,
    pub active: bool,
    pub transport: TransportMode,
    pub base_url: String,
    pub lan_base_url: String,
    pub public_base_url: String,
    pub node_key_fingerprint: String,
    pub paired_device_count: usize,
    #[serde(default)]
    pub pairing_ticket_expires_at: Option<u64>,
    #[serde(default)]
    pub pairing_uri: Option<String>,
    pub restart_required: bool,
}

#[derive(Clone, Debug)]
pub struct TicketRecord {
    pub hash: String,
    pub expires_at_ms: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("invalid key")]
    InvalidKey,
    #[error("invalid pairing uri")]
    InvalidPairingUri,
    #[error("invalid wire frame")]
    InvalidFrame,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Generate a node identity. The server owns key generation; the private key is
/// persisted by the product store.
pub fn generate_node_identity(node_id: String) -> NodeIdentity {
    let mut private = [0u8; 32];
    rand::rng().fill_bytes(&mut private);
    // Raw random bytes are not a valid clamped X25519 private key. The server
    // replaces this with snow's generated keypair; this helper is kept for
    // stores that only need a stable shape.
    let public = private;
    let public_key = URL_SAFE_NO_PAD.encode(public);
    NodeIdentity {
        node_id,
        key_id: node_key_id(&public_key),
        public_key,
        private_key: URL_SAFE_NO_PAD.encode(private),
        created_at_ms: now_ms(),
    }
}

pub fn generate_node_id() -> String {
    let mut bytes = [0u8; 12];
    rand::rng().fill_bytes(&mut bytes);
    format!("node-{}", URL_SAFE_NO_PAD.encode(bytes))
}

pub fn generate_device_id() -> String {
    let mut bytes = [0u8; 12];
    rand::rng().fill_bytes(&mut bytes);
    format!("dev-{}", URL_SAFE_NO_PAD.encode(bytes))
}

pub fn generate_ticket() -> (String, TicketRecord, u64) {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let raw = URL_SAFE_NO_PAD.encode(bytes);
    let ttl = TICKET_TTL_SECONDS_DEFAULT;
    let expires = now_ms() + ttl * 1000;
    (
        raw.clone(),
        TicketRecord {
            hash: hash_secret(&raw),
            expires_at_ms: expires,
        },
        expires,
    )
}

pub fn hash_secret(raw: &str) -> String {
    let digest = Sha256::digest(raw.as_bytes());
    hex::encode(digest)
}

pub fn node_key_id(public_key: &str) -> String {
    let bytes = URL_SAFE_NO_PAD.decode(public_key).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(b"tron-node-key");
    hasher.update(&bytes);
    let digest = hasher.finalize();
    format!("sha256:{}", hex::encode(&digest[..8]))
}

pub fn node_fingerprint(public_key: &str) -> String {
    let kid = node_key_id(public_key);
    let raw = kid.trim_start_matches("sha256:").to_ascii_uppercase();
    let mut out = String::new();
    for (index, chunk) in raw.as_bytes().chunks(4).enumerate() {
        if index > 0 {
            out.push('-');
        }
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
    }
    out
}

pub fn noise_prologue(node_id: &str, product_code: &str, transport: TransportMode) -> Vec<u8> {
    let mut out = Vec::new();
    for part in [PROTOCOL, node_id, product_code, transport.as_str()] {
        if !out.is_empty() {
            out.push(0);
        }
        out.extend_from_slice(part.trim().as_bytes());
    }
    out
}

pub fn pairing_uri(
    base_url: &str,
    product_code: &str,
    product_name: &str,
    node_id: &str,
    machine_name: &str,
    public_key: &str,
    key_id: &str,
    ticket: &str,
) -> Result<String, ProtocolError> {
    pairing_uri_with_endpoints(
        base_url,
        None,
        None,
        product_code,
        product_name,
        node_id,
        machine_name,
        public_key,
        key_id,
        ticket,
    )
}

/// Pairing URI with explicit LAN and public candidates. `base_url` is the
/// preferred fallback for clients that only understand a single address.
pub fn pairing_uri_with_endpoints(
    base_url: &str,
    lan_base_url: Option<&str>,
    public_base_url: Option<&str>,
    product_code: &str,
    product_name: &str,
    node_id: &str,
    machine_name: &str,
    public_key: &str,
    key_id: &str,
    ticket: &str,
) -> Result<String, ProtocolError> {
    let mut url = url::Url::parse("tron://pair").map_err(|_| ProtocolError::InvalidPairingUri)?;
    url.query_pairs_mut()
        .append_pair("url", base_url)
        .append_pair("name", machine_name)
        .append_pair("product", product_code)
        .append_pair("productName", product_name)
        .append_pair("node", node_id)
        .append_pair("key", public_key)
        .append_pair("kid", key_id)
        .append_pair("ticket", ticket);
    if let Some(lan) = lan_base_url.map(str::trim).filter(|value| !value.is_empty()) {
        url.query_pairs_mut().append_pair("lan", lan);
    }
    if let Some(public) = public_base_url.map(str::trim).filter(|value| !value.is_empty()) {
        url.query_pairs_mut().append_pair("public", public);
    }
    Ok(url.to_string())
}

pub fn base_url_for(settings: &RemoteSettings, port: u16) -> String {
    if settings.is_public() {
        return settings
            .public_base_url
            .trim()
            .trim_end_matches('/')
            .to_string();
    }
    format!("http://127.0.0.1:{port}")
}

pub fn local_lan_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(addr) if !addr.is_loopback() => Some(std::net::IpAddr::V4(addr)),
        other => Some(other),
    }
}

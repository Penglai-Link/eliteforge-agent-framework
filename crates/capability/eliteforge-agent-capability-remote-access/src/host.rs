//! Host contract for managing a product's remote access.
//!
//! The host owns configuration storage, the remote listener, pairing, token
//! generation and persistence. The capability pack only validates model
//! arguments, checks cancellation and projects the host answer into a safe tool
//! output. A product that reuses this pack implements this trait against its own
//! settings and never exposes the pairing token through the agent.

use async_trait::async_trait;
use serde_json::Value;

/// Remote-access operations that a product may expose to the agent.
///
/// Every method returns a JSON object. Success is `Ok(value)`; a user-readable
/// failure is `Err(message)`. Do not put a pairing token in an error message.
///
/// Recommended `status` shape (extra fields are allowed):
///
/// ```json
/// {
///   "enabled": true,
///   "active": true,
///   "restartRequired": false,
///   "tokenConfigured": true,
///   "productCode": "tron",
///   "productName": "Tron",
///   "nodeId": "node-...",
///   "machineName": "MacBook Pro",
///   "baseUrl": "https://example.test:8443",
///   "publicBaseUrl": "https://example.test",
///   "port": 8443,
///   "allowInsecureHttp": false
/// }
/// ```
///
/// When the host returns a token field or a pairing URI anyway, the capability
/// masks the token and removes the pairing URI before the agent sees the
/// result. Hosts should still prefer to omit both.
#[async_trait]
pub trait RemoteAccessHost: Send + Sync {
    /// Current remote-access settings and listener state.
    async fn status(&self) -> Result<Value, String>;

    /// Pairing information without the secret: address, product, node, and
    /// whether a token is configured. The pack returns only these safe fields
    /// and replaces any URI or token it finds with a pointer to the desktop
    /// settings page.
    async fn pairing(&self) -> Result<Value, String>;

    /// Rotate the pairing token and return the new status snapshot.
    ///
    /// Existing clients stop working until they pair again. The pack masks the
    /// new token before returning the snapshot to the agent.
    async fn rotate_token(&self) -> Result<Value, String>;

    /// Enable or disable remote access and return the resulting status snapshot.
    async fn set_enabled(&self, enabled: bool) -> Result<Value, String>;
}

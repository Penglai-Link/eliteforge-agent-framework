//! The model-facing `remote_access` tool and its output projection.
//!
//! The tool is the only agent entry point into the product's remote-access
//! control plane. It refuses state-changing actions without `confirm: true`
//! and never returns a pairing token or a pairing URI to the agent.

use std::sync::Arc;

use async_trait::async_trait;
use eliteforge_agent_core::{
    AgentError, RunContext, Tool, ToolBinding, ToolDescriptor, ToolError, ToolLease,
    ToolOperationKind, ToolOutput, ToolProvider,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::host::RemoteAccessHost;

/// Stable host/catalog identity of the management tool.
pub const TOOL_ID: &str = "framework.remote-access.manage";
/// Model-visible function name of the management tool.
pub const TOOL_CALL_NAME: &str = "remote_access";
/// Model-facing description. It states the product scope and confirmation rule.
pub const TOOL_DESCRIPTION: &str = "管理 PersonAI 产品（Tron 等）的远程访问：status 查看状态，pairing 查看配对地址与节点信息，rotate 轮换配对令牌，enable 启用远程访问，disable 停用远程访问。rotate、enable、disable 必须先获得用户明确同意并传 confirm: true。配对二维码和令牌只在桌面端设置页展示，本工具不返回令牌或带令牌的配对 URI。";

/// Replacement used for any token-like value.
const MASK: &str = "***";

/// Shown instead of a pairing URI or token.
const PAIRING_INSTRUCTIONS: &str =
    "请在桌面端设置页查看配对二维码或令牌；Agent 不会显示、复述或转发令牌。";

/// JSON Schema for the `remote_access` arguments.
pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["status", "pairing", "rotate", "enable", "disable"],
                "description": "要执行的操作。rotate/enable/disable 会改变远程访问状态或令牌。"
            },
            "confirm": {
                "type": "boolean",
                "description": "rotate、enable、disable 必须显式传 true，表示用户已明确同意该操作。其余操作忽略此字段。"
            }
        },
        "required": ["action"],
        "additionalProperties": false
    })
}

/// Descriptor registered for [`TOOL_CALL_NAME`].
///
/// `ToolOperationKind::Command` declares that the tool may change product
/// configuration. The pack additionally enforces `confirm: true` in the
/// executor because the framework operation kind is a declaration only.
pub fn tool_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(TOOL_ID, TOOL_CALL_NAME, TOOL_DESCRIPTION, input_schema())
        .with_operation_kind(ToolOperationKind::Command)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Action {
    Status,
    Pairing,
    Rotate,
    Enable,
    Disable,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Pairing => "pairing",
            Self::Rotate => "rotate",
            Self::Enable => "enable",
            Self::Disable => "disable",
        }
    }

    fn requires_confirmation(self) -> bool {
        matches!(self, Self::Rotate | Self::Enable | Self::Disable)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    action: Action,
    #[serde(default)]
    confirm: Option<bool>,
}

/// Tool provider contributed by the remote-access capability pack.
pub struct RemoteAccessToolProvider {
    host: Arc<dyn RemoteAccessHost>,
}

impl RemoteAccessToolProvider {
    pub fn new(host: Arc<dyn RemoteAccessHost>) -> Self {
        Self { host }
    }
}

#[async_trait]
impl ToolProvider for RemoteAccessToolProvider {
    fn descriptors(&self) -> Option<Vec<ToolDescriptor>> {
        Some(vec![tool_descriptor()])
    }

    async fn lease(&self, run: &RunContext) -> Result<ToolLease, AgentError> {
        run.check_cancelled()?;
        Ok(ToolLease::new(vec![ToolBinding::new(
            tool_descriptor(),
            RemoteAccessTool {
                host: self.host.clone(),
            },
        )]))
    }
}

struct RemoteAccessTool {
    host: Arc<dyn RemoteAccessHost>,
}

#[async_trait]
impl Tool for RemoteAccessTool {
    async fn call(&self, arguments: Value, run: &RunContext) -> Result<ToolOutput, ToolError> {
        run.check_cancelled()
            .map_err(|error| ToolError::Failed(error.to_string()))?;
        let arguments: Arguments = serde_json::from_value(arguments).map_err(|error| {
            ToolError::Failed(format!(
                "remote_access 参数无效：{error}。action 必须是 status、pairing、rotate、enable 或 disable。"
            ))
        })?;
        let action = arguments.action;
        if action.requires_confirmation() && arguments.confirm != Some(true) {
            return Ok(tool_output(
                Some(action),
                json!({
                    "error": format!(
                        "{} 会改变远程访问状态或配对令牌，必须先获得用户明确同意并传 confirm: true。",
                        action.as_str()
                    ),
                    "requiresConfirmation": true
                }),
                true,
            ));
        }
        let result = match action {
            Action::Status => self.host.status().await,
            Action::Pairing => self.host.pairing().await,
            Action::Rotate => self.host.rotate_token().await,
            Action::Enable => self.host.set_enabled(true).await,
            Action::Disable => self.host.set_enabled(false).await,
        };
        match result {
            Ok(value) => {
                let data = match action {
                    Action::Pairing => pairing_view(value),
                    _ => sanitize(value),
                };
                Ok(tool_output(Some(action), data, false))
            }
            Err(message) => Ok(tool_output(Some(action), json!({ "error": message }), true)),
        }
    }
}

/// Stable `{ ok, action, data }` JSON string plus the same structured value.
fn tool_output(action: Option<Action>, data: Value, is_error: bool) -> ToolOutput {
    let payload = json!({
        "ok": !is_error,
        "action": action.map(Action::as_str),
        "data": data
    });
    let content = payload.to_string();
    let mut output = if is_error {
        ToolOutput::error(content)
    } else {
        ToolOutput::text(content)
    };
    output.structured = Some(payload);
    output
}

/// Keep only safe pairing fields and point the agent to the desktop settings.
///
/// Address, product and node identity are safe to show. A token, a pairing URI
/// or a QR payload never reaches the agent even when the host returns one.
fn pairing_view(value: Value) -> Value {
    let sanitized = sanitize(value);
    let mut data = Map::new();
    if let Some(source) = sanitized.as_object() {
        for (canonical, aliases) in PAIRING_ALIASES {
            if let Some(found) = lookup(source, aliases) {
                data.insert((*canonical).to_owned(), found.clone());
            }
        }
    }
    data.insert("tokenVisibleToAgent".to_owned(), Value::Bool(false));
    data.insert(
        "instructions".to_owned(),
        Value::String(PAIRING_INSTRUCTIONS.to_owned()),
    );
    Value::Object(data)
}

/// Canonical pairing fields and the host keys they accept.
///
/// Key comparison ignores case, `_`, `-` and spaces, so `node_id` also matches
/// `nodeId`.
const PAIRING_ALIASES: &[(&str, &[&str])] = &[
    ("productCode", &["productCode", "product"]),
    ("productName", &["productName"]),
    ("nodeId", &["nodeId"]),
    ("baseUrl", &["baseUrl", "remoteBaseUrl", "address"]),
    ("publicBaseUrl", &["publicBaseUrl"]),
    ("machineName", &["machineName"]),
    ("enabled", &["enabled"]),
    ("tokenConfigured", &["tokenConfigured", "hasToken"]),
    ("allowInsecureHttp", &["allowInsecureHttp"]),
];

fn lookup<'a>(source: &'a Map<String, Value>, aliases: &[&str]) -> Option<&'a Value> {
    let aliases: Vec<String> = aliases.iter().map(|alias| normalize_key(alias)).collect();
    source
        .iter()
        .find(|(key, _)| aliases.contains(&normalize_key(key)))
        .map(|(_, value)| value)
}

/// Mask every token-like field and remove pairing URI/QR payloads at any depth.
fn sanitize(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sanitized = Map::new();
            for (key, value) in map {
                if is_pairing_uri_key(&key) {
                    continue;
                }
                let value = if is_sensitive_key(&key) {
                    mask_sensitive(value)
                } else {
                    sanitize(value)
                };
                sanitized.insert(key, value);
            }
            Value::Object(sanitized)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sanitize).collect()),
        Value::String(text) => Value::String(redact_token_text(&text)),
        other => other,
    }
}

/// Booleans stay visible: `tokenConfigured: true` answers "is a token set?".
fn mask_sensitive(value: Value) -> Value {
    match value {
        Value::Bool(_) | Value::Null => value,
        Value::String(text) if text.trim().is_empty() => Value::String(text),
        _ => Value::String(MASK.to_owned()),
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized = normalize_key(key);
    [
        "token",
        "secret",
        "password",
        "credential",
        "authorization",
        "jwt",
        "bearer",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

fn is_pairing_uri_key(key: &str) -> bool {
    let normalized = normalize_key(key);
    ["pairinguri", "qrcode", "qrurl", "tokenuri", "tokenurl"]
        .iter()
        .any(|needle| normalized.contains(needle))
}

fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|character| !matches!(character, '_' | '-' | ' '))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Replace the value of any `token=` / `access_token=` query parameter in a
/// string. This catches tokens embedded in a URL or message rather than in a
/// dedicated token field.
fn redact_token_text(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    if !lower.contains("token") {
        return text.to_owned();
    }
    let mut output = String::with_capacity(text.len());
    let mut index = 0;
    while index < text.len() {
        let Some((offset, needle_len)) = find_token_assignment(&lower[index..]) else {
            output.push_str(&text[index..]);
            break;
        };
        let value_start = index + offset + needle_len;
        output.push_str(&text[index..value_start]);
        output.push_str(MASK);
        let mut cursor = value_start;
        while cursor < text.len() {
            let byte = text.as_bytes()[cursor];
            if matches!(
                byte,
                b'&' | b'#' | b'?' | b'"' | b'\'' | b' ' | b'\t' | b'\n' | b'\r' | b','
            ) {
                break;
            }
            cursor += 1;
        }
        index = cursor;
    }
    output
}

fn find_token_assignment(text: &str) -> Option<(usize, usize)> {
    const NEEDLES: [&str; 4] = ["access_token%3d", "access_token=", "token%3d", "token="];
    NEEDLES
        .iter()
        .filter_map(|needle| text.find(needle).map(|offset| (offset, needle.len())))
        .min_by_key(|(offset, _)| *offset)
}

//! Stable two-tool gateway contracts. `tool_search` discovers the authorized
//! tool inventory as plain output; `tool_execute` wraps every other tool's
//! execution as arguments. Hosts opt in by installing the gateway bindings
//! (see the official tools-discovery plugin). The execution loop unwraps
//! `tool_execute` envelopes at admission so inner tools keep their own
//! schemas, policies, middleware, executors and audit identity.
//!
//! Discovery keeps the host's capability grouping: a binding labeled with a
//! domain id is listed by `tool_search` under that `domainCode`, and hosts
//! render the same grouping into their instructions so the model knows the
//! scope of its abilities before it starts searching.
//!
//! Cache-stability invariant: while the gateway is installed the model-visible
//! `ChatRequest.tools` list is exactly the two descriptors below for the whole
//! run. Discovery results are tool output, never exposure effects.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Stable host identity of the inventory-search tool.
pub const TOOL_SEARCH_ID: &str = "framework.tools.search";
/// Model-visible call name of the inventory-search tool.
pub const TOOL_SEARCH_CALL_NAME: &str = "tool_search";
/// Stable host identity of the execution gateway tool.
pub const TOOL_EXECUTE_ID: &str = "framework.tools.execute";
/// Model-visible call name of the execution gateway tool.
pub const TOOL_EXECUTE_CALL_NAME: &str = "tool_execute";

/// Atomic inventory-search descriptor. Gateway model projection wraps queries
/// in `items`; that projected schema stays fixed for the whole run.
///
/// `domainCode` and `query` are both optional so either entry point works:
/// the instructions list the capability groups, a group id lists that group's
/// tools, and a query narrows by words. Use `tool` alone to fetch one exact definition.
pub fn search_descriptor() -> crate::ToolDescriptor {
    crate::ToolDescriptor::new(
        TOOL_SEARCH_ID,
        TOOL_SEARCH_CALL_NAME,
        "Discover authorized tools in two stages. Pass domainCode and/or query to get compact call_name and description summaries, without schemas (query words use AND matching). Then pass tool with an exact call_name to fetch its full input_schema before execution. Do not combine tool with domainCode, query or limit. Batch queries and exact definition requests in items; reuse definitions already in context. Run tools through tool_execute using the returned schema.",
        serde_json::json!({
            "type": "object", "additionalProperties": false,
            "properties": {
                "domainCode": {"type": "string", "minLength": 1, "maxLength": 128},
                "query": {"type": "string", "minLength": 1, "maxLength": 512},
                "limit": {"type": "integer", "minimum": 1, "maximum": 20},
                "tool": {"type": "string", "minLength": 1, "maxLength": 256}
            },
            "oneOf": [
                {"required": ["tool"], "not": {"anyOf": [
                    {"required": ["domainCode"]}, {"required": ["query"]}, {"required": ["limit"]}
                ]}},
                {"not": {"required": ["tool"]}, "anyOf": [
                    {"required": ["domainCode"]}, {"required": ["query"]}
                ]}
            ]
        }),
    ).with_display_resource(include_str!("tool-display.json"))
    .with_operation_kind(crate::ToolOperationKind::Query)
}

/// Descriptor of the stable execution gateway tool. Its schema stays fixed
/// for the whole run. Batch contracts live in the discovered inner schemas.
pub fn execute_descriptor() -> crate::ToolDescriptor {
    crate::ToolDescriptor::new(
        TOOL_EXECUTE_ID,
        TOOL_EXECUTE_CALL_NAME,
        "Execute a tool discovered through tool_search. Pass {\"tool\":\"call_name\",\"arguments\":{...}} directly; tool and arguments are siblings, not inside another arguments object. Most tools require arguments.items: combine all known independent operations of that tool into one array, even for one item. Only tools explicitly declaring a single-operation schema accept plain arguments. Results preserve input order; inspect every item error.",
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["tool", "arguments"],
            "properties": {
                "tool": {"type": "string", "minLength": 1, "maxLength": 256},
                "arguments": {"type": "object"}
            }
        }),
    ).with_display_resource(include_str!("tool-display.json"))
    .with_operation_kind(crate::ToolOperationKind::Command)
    .with_single_input()
}

/// True when the call name belongs to the stable gateway pair. Hosts use this
/// to keep gateway bindings out of inner-tool projections.
pub fn is_gateway_call_name(call_name: &str) -> bool {
    call_name == TOOL_SEARCH_CALL_NAME || call_name == TOOL_EXECUTE_CALL_NAME
}

/// True for the two stable gateway descriptors, never for wrapped inner tools.
pub fn is_gateway_descriptor(descriptor: &crate::ToolDescriptor) -> bool {
    matches!(
        (descriptor.id.as_str(), descriptor.call_name.as_str()),
        (TOOL_SEARCH_ID, TOOL_SEARCH_CALL_NAME) | (TOOL_EXECUTE_ID, TOOL_EXECUTE_CALL_NAME)
    )
}

/// A parsed `tool_execute` envelope: the inner tool's call name plus the
/// arguments object addressed to it.
#[derive(Debug, Clone, PartialEq)]
pub struct GatewayEnvelope {
    pub tool: String,
    pub arguments: Value,
}

impl GatewayEnvelope {
    /// Parse the arguments of a `tool_execute` call.
    ///
    /// The declared schema is `{"tool": string, "arguments": object}`. Parsing
    /// is deliberately lenient about shapes providers produce in practice:
    /// `input`/`args` are accepted as aliases of `arguments`, a JSON-encoded
    /// string is decoded, and a missing `arguments` key tolerates the spread
    /// form `{"tool": "...", ...inner arguments}`.
    pub fn parse(arguments: &Value) -> Result<Self, String> {
        let Value::Object(map) = arguments else {
            return Err("tool_execute arguments must be an object".into());
        };
        let tool = match map.get("tool") {
            Some(Value::String(name)) if !name.trim().is_empty() => name.trim().to_owned(),
            _ => return Err(
                "tool_execute requires a nonempty string \"tool\" field at the top level. Pass {\"tool\":\"call_name\",\"arguments\":{...}} directly; do not wrap this envelope in another arguments object. No tool was executed; correct the envelope and retry.".into()
            ),
        };
        let raw = ["arguments", "input", "args"]
            .iter()
            .find_map(|key| map.get(*key));
        let arguments = match raw {
            None | Some(Value::Null) => {
                let mut spread = map.clone();
                spread.remove("tool");
                for alias in ["arguments", "input", "args"] {
                    spread.remove(alias);
                }
                Value::Object(spread)
            }
            Some(Value::Object(object)) => Value::Object(object.clone()),
            Some(Value::String(encoded)) => serde_json::from_str(encoded)
                .map_err(|e| format!("tool_execute arguments must be a JSON object: {e}"))?,
            Some(_) => return Err("tool_execute arguments must be an object".into()),
        };
        if !arguments.is_object() {
            return Err("tool_execute arguments must be an object".into());
        }
        Ok(Self { tool, arguments })
    }

    /// Rewrite a provider `tool_execute` call into the inner tool's call.
    /// The provider call id and invocation identity are preserved so tool
    /// result messages still pair with the assistant's original call.
    pub fn unwrap_call(&self, call: &crate::ToolCall) -> crate::ToolCall {
        crate::ToolCall {
            invocation_id: call.invocation_id,
            id: call.id.clone(),
            call_name: self.tool.clone(),
            arguments: self.arguments.clone(),
        }
    }
}

/// Presentation helper for protocols that show the model's tool call to a
/// human (AG-UI events, host transcripts, short-term memory blocks): display
/// the inner tool name and arguments instead of the gateway envelope.
///
/// Returns `None` when the call is not a `tool_execute` envelope or the
/// envelope cannot be parsed; callers then present the original call.
pub fn presented_call(call: &crate::ToolCall) -> Option<crate::ToolCall> {
    if call.call_name != TOOL_EXECUTE_CALL_NAME {
        return None;
    }
    GatewayEnvelope::parse(&call.arguments)
        .ok()
        .map(|envelope| envelope.unwrap_call(call))
}

/// One searchable entry of the wrapped inventory: the tool's descriptor plus
/// the host capability group it belongs to, when the host labeled the binding
/// with one. Unlabeled tools stay searchable by query and are simply absent
/// from any `domainCode` listing.
#[derive(Debug, Clone, PartialEq)]
pub struct GatewayInventoryEntry {
    pub descriptor: crate::ToolDescriptor,
    pub domain_code: Option<String>,
    /// Original description, before the execution loop adds batch instructions.
    pub summary_description: Option<String>,
}

impl GatewayInventoryEntry {
    pub fn new(descriptor: crate::ToolDescriptor) -> Self {
        Self {
            descriptor,
            domain_code: None,
            summary_description: None,
        }
    }

    pub fn with_summary_description(mut self, description: impl Into<String>) -> Self {
        self.summary_description = Some(description.into());
        self
    }

    pub fn with_domain_code(mut self, domain_code: impl Into<String>) -> Self {
        self.domain_code = Some(domain_code.into());
        self
    }
}

/// Executor of the inventory-search tool. The entry list is the wrapped
/// inventory the host authorized; hosts normally let the execution loop
/// re-mint this binding with the complete leased catalog so search covers
/// every wrapped tool, not just one plugin's contribution.
pub struct GatewaySearch {
    pub entries: Vec<GatewayInventoryEntry>,
}

/// Compact model-facing discovery result shared by hosts and presentation clients.
/// Exact definitions retain the complete schema, including local references.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSearchEntry {
    pub call_name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolSearchMode {
    Summary,
    Definition,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename = "toolSearch")]
pub struct ToolSearchResult {
    pub mode: ToolSearchMode,
    pub tools: Vec<ToolSearchEntry>,
    pub matched: usize,
    pub truncated: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct GatewaySearchInput {
    domain_code: Option<String>,
    query: Option<String>,
    limit: Option<usize>,
    tool: Option<String>,
}

#[async_trait::async_trait]
impl crate::Tool for GatewaySearch {
    async fn call(
        &self,
        arguments: Value,
        context: &crate::RunContext,
    ) -> Result<crate::ToolOutput, crate::ToolError> {
        let result = self.search(arguments, &|| context.is_cancelled(), &|id| {
            context.is_tool_revoked(id)
        })?;
        let data =
            serde_json::to_value(result).map_err(|e| crate::ToolError::Failed(e.to_string()))?;
        let mut output = crate::ToolOutput::text(data.to_string());
        output.structured = Some(data);
        Ok(output)
    }
}

impl GatewaySearch {
    /// Both query modes resolve against the same authorized, current inventory.
    pub fn search(
        &self,
        arguments: Value,
        is_cancelled: &dyn Fn() -> bool,
        is_revoked: &dyn Fn(&str) -> bool,
    ) -> Result<ToolSearchResult, crate::ToolError> {
        let fail = |message: &str| crate::ToolError::Failed(message.into());
        let input: GatewaySearchInput = serde_json::from_value(arguments)
            .map_err(|e| crate::ToolError::Failed(e.to_string()))?;
        if is_cancelled() {
            return Err(fail("tool search cancelled"));
        }
        if let Some(tool) = input.tool {
            if input.domain_code.is_some() || input.query.is_some() || input.limit.is_some() {
                return Err(fail(
                    "Use tool alone for an exact definition; use domainCode/query for summaries",
                ));
            }
            if tool.trim().is_empty() || tool.len() > 256 {
                return Err(fail("tool must be a nonempty exact call_name"));
            }
            let entry = self.entries.iter().find(|entry| entry.descriptor.call_name == tool && !is_revoked(&entry.descriptor.id))
                .ok_or_else(|| fail("Tool definition unavailable; search the authorized inventory for an exact call_name"))?;
            return Ok(ToolSearchResult {
                mode: ToolSearchMode::Definition,
                tools: vec![ToolSearchEntry {
                    call_name: entry.descriptor.call_name.clone(),
                    description: entry
                        .summary_description
                        .as_ref()
                        .unwrap_or(&entry.descriptor.description)
                        .clone(),
                    input_schema: Some(entry.descriptor.input_schema.clone()),
                }],
                matched: 1,
                truncated: false,
            });
        }
        let limit = input.limit.unwrap_or(5);
        if !(1..=20).contains(&limit) {
            return Err(fail("invalid query or limit"));
        }
        let domain = input.domain_code.as_deref().map(str::trim);
        if domain.is_some_and(|value| value.is_empty() || value.len() > 128) {
            return Err(fail("invalid domainCode"));
        }
        if input.query.as_ref().is_some_and(|value| value.len() > 512) {
            return Err(fail("invalid query"));
        }
        let terms: Vec<_> = input
            .query
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        if domain.is_none() && terms.is_empty() {
            return Err(fail(
                "tool_search requires a nonempty domainCode or query, or an exact tool",
            ));
        }
        let (selected, matched) = Self::select(
            &self.entries,
            domain.map(str::to_lowercase).as_deref(),
            &terms,
            limit,
            is_cancelled,
            is_revoked,
        )?;
        Ok(ToolSearchResult {
            mode: ToolSearchMode::Summary,
            tools: selected,
            matched,
            truncated: matched > limit,
        })
    }

    fn select(
        entries: &[GatewayInventoryEntry],
        domain: Option<&str>,
        terms: &[String],
        limit: usize,
        is_cancelled: &dyn Fn() -> bool,
        is_revoked: &dyn Fn(&str) -> bool,
    ) -> Result<(Vec<ToolSearchEntry>, usize), crate::ToolError> {
        let mut selected = Vec::new();
        let mut matches = 0;
        for entry in entries {
            if is_cancelled() {
                return Err(crate::ToolError::Failed("tool search cancelled".into()));
            }
            let descriptor = &entry.descriptor;
            if is_revoked(&descriptor.id) {
                continue;
            }
            if domain.is_some_and(|domain| {
                entry
                    .domain_code
                    .as_deref()
                    .map(str::to_lowercase)
                    .as_deref()
                    != Some(domain)
            }) {
                continue;
            }
            let description = entry
                .summary_description
                .as_deref()
                .unwrap_or(&descriptor.description);
            let haystack = format!("{} {} {}", descriptor.id, descriptor.call_name, description)
                .to_lowercase();
            if !terms.iter().all(|term| haystack.contains(term)) {
                continue;
            }
            matches += 1;
            if selected.len() < limit {
                selected.push(ToolSearchEntry {
                    call_name: descriptor.call_name.clone(),
                    description: if description.chars().count() > 512 {
                        format!("{}…", description.chars().take(512).collect::<String>())
                    } else {
                        description.to_owned()
                    },
                    input_schema: None,
                });
            }
        }
        Ok((selected, matches))
    }
}

/// Host-owned capability-group labels for gateway discovery.
///
/// The framework never interprets the code: it copies the host's label onto
/// each binding it snapshots into the `tool_search` inventory, so a host can
/// group official, product and plugin tools by its own taxonomy without the
/// framework knowing that taxonomy. Hosts publish one map on the run request
/// (`RunRequest::extensions`, which the run context inherits).
///
/// A key is either a tool descriptor id or its call name; the id wins, because
/// call names can be aliased per run. A binding's own `domain_code` always
/// wins over this map.
#[derive(Debug, Clone, Default)]
pub struct ToolGroupLabels(std::collections::BTreeMap<String, String>);

impl ToolGroupLabels {
    pub fn new() -> Self {
        Self::default()
    }

    /// Label one tool. Blank keys and blank codes are ignored, so hosts can
    /// project an optional catalog field without filtering first.
    pub fn set(&mut self, key: impl Into<String>, group_code: impl Into<String>) -> &mut Self {
        let key = key.into();
        let group_code = group_code.into();
        if !key.trim().is_empty() && !group_code.trim().is_empty() {
            self.0.insert(key, group_code);
        }
        self
    }

    /// Builder form for host setup code.
    pub fn with(mut self, key: impl Into<String>, group_code: impl Into<String>) -> Self {
        self.set(key, group_code);
        self
    }

    /// Group code for a tool, preferring the stable descriptor id.
    pub fn label_for(&self, id: &str, call_name: &str) -> Option<&str> {
        self.0
            .get(id)
            .or_else(|| self.0.get(call_name))
            .map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Defensive executor for the re-minted `tool_execute` binding: the loop
/// unwraps envelopes before dispatch, so this never runs. It exists so a
/// directly dispatched gateway binding fails loudly instead of succeeding.
pub struct GatewayExecute;

#[async_trait::async_trait]
impl crate::Tool for GatewayExecute {
    async fn call(
        &self,
        _: Value,
        _: &crate::RunContext,
    ) -> Result<crate::ToolOutput, crate::ToolError> {
        Err(crate::ToolError::Failed(
            "tool_execute envelopes are unwrapped before dispatch".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(arguments: Value) -> crate::ToolCall {
        let mut call = crate::ToolCall::new("provider-1", TOOL_EXECUTE_CALL_NAME, arguments);
        call.invocation_id = crate::RunId::new_v4();
        call
    }

    #[test]
    fn descriptors_are_stable_and_self_describing() {
        let search = search_descriptor();
        let execute = execute_descriptor();
        assert_eq!(search.id, TOOL_SEARCH_ID);
        assert_eq!(search.call_name, TOOL_SEARCH_CALL_NAME);
        assert_eq!(execute.id, TOOL_EXECUTE_ID);
        assert_eq!(execute.call_name, TOOL_EXECUTE_CALL_NAME);
        assert!(is_gateway_descriptor(&search));
        assert!(is_gateway_descriptor(&execute));
        assert!(!is_gateway_descriptor(&crate::ToolDescriptor::new(
            "other",
            "other",
            "other",
            json!({})
        ).with_display_resource(include_str!("tool-display.json"))));
    }

    #[test]
    fn envelope_parse_accepts_declared_and_lenient_shapes() {
        let plain = GatewayEnvelope::parse(&json!({"tool":"bash_exec","arguments":{"cmd":"ls"}}));
        assert_eq!(
            plain.unwrap(),
            GatewayEnvelope {
                tool: "bash_exec".into(),
                arguments: json!({"cmd":"ls"})
            }
        );
        let alias = GatewayEnvelope::parse(&json!({"tool":"bash_exec","input":{"cmd":"ls"}}));
        assert_eq!(alias.unwrap().arguments, json!({"cmd":"ls"}));
        let encoded =
            GatewayEnvelope::parse(&json!({"tool":"bash_exec","arguments":"{\"cmd\":\"ls\"}"}));
        assert_eq!(encoded.unwrap().arguments, json!({"cmd":"ls"}));
        let spread = GatewayEnvelope::parse(&json!({"tool":"bash_exec","cmd":"ls"}));
        assert_eq!(spread.unwrap().arguments, json!({"cmd":"ls"}));
        let empty = GatewayEnvelope::parse(&json!({"tool":"bash_exec"}));
        assert_eq!(empty.unwrap().arguments, json!({}));
    }

    #[test]
    fn envelope_parse_rejects_invalid_shapes() {
        assert!(GatewayEnvelope::parse(&json!("text")).is_err());
        assert!(GatewayEnvelope::parse(&json!({"arguments":{}})).is_err());
        assert!(GatewayEnvelope::parse(&json!({"tool":"  ","arguments":{}})).is_err());
        assert!(
            GatewayEnvelope::parse(&json!({"tool":"bash_exec","arguments":"not json"})).is_err()
        );
        assert!(GatewayEnvelope::parse(&json!({"tool":"bash_exec","arguments":[1]})).is_err());
    }

    #[test]
    fn unwrap_preserves_provider_identity() {
        let original = call(json!({"tool":"bash_exec","arguments":{"cmd":"ls"}}));
        let envelope = GatewayEnvelope::parse(&original.arguments).unwrap();
        let inner = envelope.unwrap_call(&original);
        assert_eq!(inner.id, original.id);
        assert_eq!(inner.invocation_id, original.invocation_id);
        assert_eq!(inner.call_name, "bash_exec");
        assert_eq!(inner.arguments, json!({"cmd":"ls"}));
    }

    #[test]
    fn presented_call_unwraps_only_valid_envelopes() {
        let original = call(json!({"tool":"bash_exec","arguments":{"cmd":"ls"}}));
        let presented = presented_call(&original).unwrap();
        assert_eq!(presented.call_name, "bash_exec");
        assert_eq!(presented.id, original.id);

        let direct = crate::ToolCall::new("provider-2", "bash_exec", json!({"cmd":"ls"}));
        assert!(presented_call(&direct).is_none());

        let broken = call(json!({"arguments":{}}));
        assert!(presented_call(&broken).is_none());
    }

    fn entry(
        id: &str,
        name: &str,
        description: &str,
        domain: Option<&str>,
    ) -> GatewayInventoryEntry {
        let descriptor =
            crate::ToolDescriptor::new(id, name, description, json!({"type":"object"})).with_display_resource(include_str!("tool-display.json"));
        match domain {
            Some(domain) => GatewayInventoryEntry::new(descriptor).with_domain_code(domain),
            None => GatewayInventoryEntry::new(descriptor),
        }
    }

    fn inventory() -> Vec<GatewayInventoryEntry> {
        let mut entries = vec![
            entry(
                "tron.worktree",
                "worktree_create",
                "Create a git worktree",
                Some("worktree"),
            ),
            entry(
                "tron.git",
                "git_switch_branch",
                "Switch the branch",
                Some("git"),
            ),
            entry(
                "tron.git",
                "git_status",
                "Show working tree status",
                Some("git"),
            ),
            entry("tron.fs", "fs_read_text", "Read a text file", None),
        ];
        // The loop re-mints the search binding from a call-name sorted catalog.
        entries.sort_by(|a, b| a.descriptor.call_name.cmp(&b.descriptor.call_name));
        entries
    }

    fn select(domain: Option<&str>, query: Option<&str>, limit: usize) -> (Vec<String>, usize) {
        let terms: Vec<_> = query
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let (selected, matches) =
            GatewaySearch::select(&inventory(), domain, &terms, limit, &|| false, &|_| false)
                .unwrap();
        (
            selected.into_iter().map(|tool| tool.call_name).collect(),
            matches,
        )
    }

    #[test]
    fn domain_code_lists_only_that_capability_group() {
        // A group id alone lists the group, regardless of descriptions.
        assert_eq!(
            select(Some("git"), None, 20),
            (vec!["git_status".into(), "git_switch_branch".into()], 2)
        );
        // `call` lowercases the incoming `domainCode` before selecting, so an
        // uppercase request still lists the group.
        assert_eq!(
            select(Some("worktree"), None, 20).0,
            vec!["worktree_create"]
        );
        // Unknown groups list nothing instead of failing the run.
        assert_eq!(select(Some("absent"), None, 20), (Vec::<String>::new(), 0));
        // Unlabeled tools stay reachable by query but never by group.
        assert_eq!(select(Some("fs"), None, 20), (Vec::<String>::new(), 0));
    }

    #[test]
    fn domain_code_and_query_combine_and_page() {
        // Query narrows inside the group; AND matching still applies.
        assert_eq!(
            select(Some("git"), Some("branch"), 20).0,
            vec!["git_switch_branch"]
        );
        assert_eq!(
            select(Some("git"), Some("status"), 20).0,
            vec!["git_status"]
        );
        // A term from another group's tool yields no match inside this group.
        assert_eq!(
            select(Some("git"), Some("worktree"), 20),
            (Vec::<String>::new(), 0)
        );
        // limit pages the selection while `matched` still reports every match.
        assert_eq!(select(Some("git"), None, 1), (vec!["git_status".into()], 2));
        // The query-only path is unchanged from the pre-domain contract.
        assert_eq!(select(None, Some("text file"), 20).0, vec!["fs_read_text"]);
        assert_eq!(
            select(None, Some("read"), 20),
            (vec!["fs_read_text".into()], 1)
        );
    }

    #[test]
    fn revoked_tools_drop_out_of_group_listings() {
        let (selected, matches) =
            GatewaySearch::select(&inventory(), Some("git"), &[], 20, &|| false, &|id| {
                id == "tron.git"
            })
            .unwrap();
        assert!(selected.is_empty());
        assert_eq!(matches, 0);
    }

    #[test]
    fn host_group_labels_are_opaque_and_id_wins() {
        let labels = ToolGroupLabels::new()
            .with("tron.git", "git")
            .with("git_switch_branch", "legacy")
            // Blank projections from an optional catalog column are ignored.
            .with("", "git")
            .with("git_status", "  ");
        assert!(!labels.is_empty());
        // The stable id beats a call-name alias for the same tool.
        assert_eq!(
            labels.label_for("tron.git", "git_switch_branch"),
            Some("git")
        );
        // Call-name keys still work when the host has no id.
        assert_eq!(
            labels.label_for("unknown", "git_switch_branch"),
            Some("legacy")
        );
        assert_eq!(labels.label_for("git_status", "git_status"), None);
        assert_eq!(labels.label_for("nothing", "nothing"), None);
        assert!(ToolGroupLabels::new().is_empty());
        assert_eq!(ToolGroupLabels::new().label_for("a", "b"), None);
    }

    #[test]
    fn search_schema_advertises_the_domain_entry_point() {
        let schema = search_descriptor().input_schema;
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(schema["properties"]["domainCode"]["type"], json!("string"));
        assert_eq!(schema["properties"]["query"]["type"], json!("string"));
        // Neither is individually required: the model may start from the group
        // list in its instructions or from a keyword.
        assert!(schema.get("required").is_none());
    }
    #[test]
    fn two_stage_search_keeps_summaries_small_and_definitions_exact() {
        let gateway = GatewaySearch {
            entries: inventory(),
        };
        let query = json!({"domainCode":"git","limit":1});
        let summary = gateway.search(query, &|| false, &|_| false).unwrap();
        assert_eq!(summary.mode, ToolSearchMode::Summary);
        assert_eq!(summary.matched, 2);
        assert!(summary.truncated);
        assert!(summary.tools.iter().all(|t| t.input_schema.is_none()));
        assert!(
            !serde_json::to_string(&summary)
                .unwrap()
                .contains("input_schema")
        );
        let definition = gateway
            .search(json!({"tool":"git_status"}), &|| false, &|_| false)
            .unwrap();
        assert_eq!(definition.mode, ToolSearchMode::Definition);
        assert!(definition.tools[0].input_schema.is_some());
        assert!(
            gateway
                .search(json!({"tool":"git_status"}), &|| false, &|id| id
                    == "tron.git")
                .is_err()
        );
        assert!(
            gateway
                .search(json!({"tool":"git_status"}), &|| true, &|_| false)
                .is_err()
        );
        assert!(
            gateway
                .search(json!({"tool":"git"}), &|| false, &|_| false)
                .is_err()
        );
        let validator = crate::compile_schema(&search_descriptor().input_schema).unwrap();
        for invalid in [
            json!({}),
            json!({"tool":"git_status","query":"git"}),
            json!({"tool":"git_status","limit":1}),
            json!({"tool":""}),
        ] {
            assert!(!validator.is_valid(&invalid));
            assert!(gateway.search(invalid, &|| false, &|_| false).is_err());
        }
        assert!(validator.is_valid(&json!({"tool":"git_status"})));
        assert!(validator.is_valid(&json!({"domainCode":"git","query":"status"})));
    }
}

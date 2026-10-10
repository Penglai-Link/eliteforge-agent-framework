//! A2UI v0.9.1: validated UI data, durable session state and typed user input.
//! HTTP, product authorization and rendering stay in the host.
use async_trait::async_trait;
use eliteforge_agent_core::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, OnceLock},
};
pub const VERSION: &str = "v0.9.1";
pub const MINIMAL_CATALOG_ID: &str = "https://eliteforge.dev/a2ui/v0.9.1/minimal";
pub const AGENT_CATALOG_ID: &str = "https://eliteforge.dev/a2ui/v0.9.1/agent";
mod guidance;
pub use guidance::{
    A2UI_EXPAND_TOOL, A2UI_PACK_ID, A2UI_SKILL_NAME, A2uiPack, expand_descriptor, is_a2ui_tool,
    negotiated,
};
const KEY: &str = "framework.a2ui.v1";
const MAX_BYTES: usize = 1024 * 1024;
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct A2uiClientCapabilities {
    pub version: String,
    pub catalog_ids: Vec<String>,
}
#[derive(Debug, Clone)]
pub struct A2uiRunOptions {
    pub capabilities: A2uiClientCapabilities,
    pub owner_agent_id: String,
}
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq)]
pub enum SurfaceMode {
    Display,
    SubmitOnce,
    Reusable,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SurfaceSnapshot {
    pub surface_id: String,
    pub catalog_id: String,
    pub send_data_model: bool,
    pub components: BTreeMap<String, Value>,
    pub data_model: Value,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SurfaceRecord {
    pub surface_id: String,
    pub surface_instance_id: String,
    pub revision: u64,
    pub mode: SurfaceMode,
    pub owner_agent_id: String,
    pub run_id: String,
    pub message_id: String,
    pub created_at_ms: u64,
    pub snapshot: SurfaceSnapshot,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct A2uiState {
    pub session_id: String,
    pub revision: u64,
    pub surfaces: Vec<SurfaceRecord>,
    pub pending_interactions: Vec<InteractionSurface>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractionSurface {
    pub interaction_id: String,
    pub surface_instance_id: String,
    pub run_id: String,
    pub mode: SurfaceMode,
    pub messages: Vec<Value>,
    pub expires_at_ms: Option<u64>,
}
/// A pure projection over the native record. It never creates approval authority or answer state.
pub fn project_interaction(record: &InteractionRecord) -> InteractionSurface {
    let request = &record.request;
    let id = format!("interaction:{}", request.id);
    let original = request.presentation.as_ref();
    let messages = if original.is_some_and(|p| p["kind"] == "a2ui") {
        original
            .and_then(|p| p["messages"].as_array())
            .cloned()
            .unwrap_or_default()
    } else {
        let approval = matches!(request.kind, InteractionKind::Approval { .. });
        let questions=original.and_then(|p|p["questions"].as_array()).map(|questions|questions.iter().map(|q|json!({"questionId":q["id"],"question":q["question"],"options":q["options"].as_array().map(|o|o.iter().map(|o|json!({"optionId":o["id"],"label":o["label"],"description":o["description"]})).collect::<Vec<_>>()).unwrap_or_default(),"allowCustomAnswer":q["allow_free_text"].as_bool().unwrap_or(true),"allowSkip":q["allow_skip"].as_bool().unwrap_or(true)})).collect::<Vec<_>>()).unwrap_or_else(||vec![json!({"questionId":request.id,"question":request.prompt.text(),"options":[],"allowCustomAnswer":true,"allowSkip":false})]);
        let mut component = json!({"id":"root","component":if approval{"ApprovalCard"}else{"QuestionCard"},"intro":request.prompt.text(),"value":{"path":"/answer"},"status":{"path":"/status"},"labels":{},"action":{"event":{"name":"interaction.submit","context":{"answer":{"path":"/answer"}}}}});
        if !approval {
            component["questions"] = json!(questions);
        }
        if let Some(expiry) = request.expires_at_unix_ms {
            component["expiresAtMs"] = json!(expiry);
        }
        if let Some(call) = &record.tool_call {
            component["details"] = json!({"tool":call.call_name,"arguments":call.arguments});
            if !approval {
                component.as_object_mut().unwrap().remove("details");
            }
        }
        vec![
            json!({"version":VERSION,"createSurface":{"surfaceId":id,"catalogId":AGENT_CATALOG_ID}}),
            json!({"version":VERSION,"updateComponents":{"surfaceId":id,"components":[component]}}),
            json!({"version":VERSION,"updateDataModel":{"surfaceId":id,"value":{"answer":if approval{Value::Null}else{json!({})},"status":"Pending"}}}),
        ]
    };
    InteractionSurface {
        interaction_id: request.id.to_string(),
        surface_instance_id: request.id.to_string(),
        run_id: record.run_id.to_string(),
        mode: SurfaceMode::SubmitOnce,
        messages,
        expires_at_ms: request.expires_at_unix_ms,
    }
}
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionState {
    #[serde(default)]
    publication_inputs: BTreeMap<String, Value>,
    revision: u64,
    surfaces: BTreeMap<String, SurfaceRecord>,
    publications: BTreeMap<String, Value>,
    actions: BTreeMap<String, ActionReceipt>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionInput {
    pub action_id: String,
    pub surface_instance_id: String,
    pub expected_revision: u64,
    pub message: Value,
    pub metadata: Value,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionReceipt {
    pub input: ActionInput,
    pub run_id: String,
    pub owner_agent_id: String,
    #[serde(default)]
    pub claimed: bool,
    #[serde(default)]
    pub completion: Option<Value>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PublishInput {
    mode: SurfaceMode,
    messages: Vec<Value>,
    response_schema: Option<Value>,
}
#[derive(Clone)]
pub struct A2uiService {
    interactions: Arc<OnceLock<Arc<dyn InteractionStore>>>,
    store: Arc<OnceLock<Arc<dyn SessionLifecycleStore>>>,
    validators: Arc<BTreeMap<String, jsonschema::Validator>>,
}
fn invalid(message: impl Into<String>) -> AgentError {
    AgentError::InvalidConfiguration(message.into())
}
fn safe(value: &Value) -> bool {
    match value {
        Value::Object(m) => m.iter().all(|(k, v)| {
            !["__proto__", "constructor", "prototype"].contains(&k.as_str()) && safe(v)
        }),
        Value::Array(v) => v.iter().all(safe),
        _ => true,
    }
}
fn rewrite(value: &mut Value, name: &str) {
    match value {
        Value::Object(map) => {
            map.remove("$id");
            if let Some(Value::String(reference)) = map.get_mut("$ref") {
                if let Some(path) =
                    reference.strip_prefix("https://a2ui.org/specification/v0_9/common_types.json#")
                {
                    *reference = format!("#/$defs/common{path}");
                } else if let Some(path) = reference.strip_prefix("catalog.json#") {
                    *reference = format!("#/$defs/catalog{path}");
                } else if let Some(path) = reference.strip_prefix('#') {
                    *reference = format!("#/$defs/{name}{path}");
                }
            }
            for child in map.values_mut() {
                rewrite(child, name);
            }
        }
        Value::Array(items) => {
            for item in items {
                rewrite(item, name);
            }
        }
        _ => {}
    }
}
impl A2uiService {
    pub fn new() -> Result<Self, AgentError> {
        let mut validators = BTreeMap::new();
        for (id, source) in [
            (MINIMAL_CATALOG_ID, include_str!("schemas/catalog.json")),
            (AGENT_CATALOG_ID, include_str!("schemas/agent_catalog.json")),
        ] {
            let mut common: Value = serde_json::from_str(include_str!("schemas/common_types.json"))
                .map_err(|e| invalid(e.to_string()))?;
            let mut catalog: Value =
                serde_json::from_str(source).map_err(|e| invalid(e.to_string()))?;
            let mut envelope: Value =
                serde_json::from_str(include_str!("schemas/server_to_client.json"))
                    .map_err(|e| invalid(e.to_string()))?;
            rewrite(&mut common, "common");
            rewrite(&mut catalog, "catalog");
            rewrite(&mut envelope, "envelope");
            validators.insert(id.to_owned(), compile_schema(&json!({"$schema":"https://json-schema.org/draft/2020-12/schema", "$ref":"#/$defs/envelope", "$defs":{"common":common,"catalog":catalog,"envelope":envelope}}))?);
        }
        Ok(Self {
            interactions: Arc::new(OnceLock::new()),
            store: Arc::new(OnceLock::new()),
            validators: Arc::new(validators),
        })
    }
    fn store(&self) -> Result<&Arc<dyn SessionLifecycleStore>, AgentError> {
        self.store
            .get()
            .ok_or_else(|| invalid("A2UI requires session lifecycle storage"))
    }
    async fn load(&self, session: &str) -> Result<(Option<u64>, SessionState), AgentError> {
        match self.store()?.read_context(session, KEY).await? {
            Some((revision, value)) => Ok((
                Some(revision),
                serde_json::from_value(value).map_err(|_| invalid("Invalid A2UI session state"))?,
            )),
            None => Ok((None, SessionState::default())),
        }
    }
    pub async fn state(&self, session: &str) -> Result<A2uiState, AgentError> {
        let (_, state) = self.load(session).await?;
        Ok(A2uiState {
            session_id: session.to_owned(),
            revision: state.revision,
            surfaces: state.surfaces.into_values().collect(),
            pending_interactions: match self.interactions.get() {
                Some(store) => store
                    .pending(session)
                    .await?
                    .iter()
                    .map(project_interaction)
                    .collect(),
                None => vec![],
            },
        })
    }
    async fn save(
        &self,
        session: &str,
        expected: Option<u64>,
        state: &SessionState,
    ) -> Result<(), AgentError> {
        let value = serde_json::to_value(state).map_err(|e| invalid(e.to_string()))?;
        // Session-level quota includes deduplication receipts. Never silently evict live state.
        if value.to_string().len() > 16 * MAX_BYTES {
            return Err(invalid("A2UI session quota exceeded"));
        }
        self.store()?
            .write_context(session, KEY, expected, value)
            .await?;
        Ok(())
    }
    pub async fn publish(
        &self,
        session: &str,
        owner: &str,
        run: &str,
        invocation: &str,
        capabilities: &A2uiClientCapabilities,
        input: Value,
    ) -> Result<Value, AgentError> {
        if capabilities.version != VERSION {
            return Err(invalid("Unsupported A2UI version"));
        }
        if input.to_string().len() > MAX_BYTES || !safe(&input) {
            return Err(invalid("Invalid A2UI payload"));
        }
        let raw_input = input.clone();
        let input: PublishInput =
            serde_json::from_value(input).map_err(|e| invalid(e.to_string()))?;
        if input.mode == SurfaceMode::SubmitOnce {
            return Err(invalid("Use a2ui_request_input for one-shot forms"));
        }
        if input.messages.is_empty() || input.messages.len() > 128 {
            return Err(invalid("Expected 1-128 A2UI envelopes"));
        }
        let (version, mut state) = self.load(session).await?;
        if let Some(receipt) = state.publications.get(invocation) {
            if state.publication_inputs.get(invocation) != Some(&raw_input) {
                return Err(StateError::Conflict.into());
            }
            return Ok(receipt.clone());
        }
        let mut id = None;
        let mut deleted = false;
        let mut last = None;
        for message in &input.messages {
            if message["version"] != VERSION {
                return Err(invalid("Expected A2UI v0.9.1"));
            }
            let (operation, payload) = message
                .as_object()
                .and_then(|m| m.iter().find(|(k, _)| k.as_str() != "version"))
                .ok_or_else(|| invalid("Missing operation"))?;
            let surface_id = payload["surfaceId"]
                .as_str()
                .filter(|id| !id.starts_with("interaction:") && !id.is_empty())
                .ok_or_else(|| invalid("Invalid or reserved surface ID"))?;
            if id
                .replace(surface_id.to_owned())
                .is_some_and(|old| old != surface_id)
            {
                return Err(invalid("One surface per publication"));
            }
            let catalog_id = if operation == "createSurface" {
                payload["catalogId"].as_str()
            } else {
                state
                    .surfaces
                    .get(surface_id)
                    .map(|s| s.snapshot.catalog_id.as_str())
            }
            .ok_or_else(|| invalid("Unknown surface"))?;
            if !capabilities.catalog_ids.iter().any(|id| id == catalog_id) {
                return Err(invalid("Catalog not negotiated"));
            }
            let validator = self
                .validators
                .get(catalog_id)
                .ok_or_else(|| invalid("Catalog not supported"))?;
            if !validator.is_valid(message) {
                return Err(invalid(format!(
                    "VALIDATION_FAILED: {}",
                    validator
                        .iter_errors(message)
                        .map(|e| e.to_string())
                        .collect::<Vec<_>>()
                        .join("; ")
                )));
            }
            if operation == "createSurface" {
                if state.surfaces.contains_key(surface_id) || state.surfaces.len() >= 64 {
                    return Err(invalid("Duplicate surface or surface quota exceeded"));
                }
                let instance = RunId::new_v4().to_string();
                state.surfaces.insert(
                    surface_id.to_owned(),
                    SurfaceRecord {
                        surface_id: surface_id.to_owned(),
                        surface_instance_id: instance.clone(),
                        revision: state.revision + 1,
                        mode: input.mode,
                        owner_agent_id: owner.to_owned(),
                        run_id: run.to_owned(),
                        message_id: format!("a2ui:{instance}"),
                        created_at_ms: unix_time_ms(),
                        snapshot: SurfaceSnapshot {
                            surface_id: surface_id.to_owned(),
                            catalog_id: catalog_id.to_owned(),
                            send_data_model: payload["sendDataModel"].as_bool().unwrap_or(false),
                            components: BTreeMap::new(),
                            data_model: json!({}),
                        },
                    },
                );
                deleted = false;
            } else {
                let surface = state
                    .surfaces
                    .get_mut(surface_id)
                    .ok_or_else(|| invalid("Unknown surface"))?;
                if surface.owner_agent_id != owner || surface.mode != input.mode {
                    return Err(invalid("Surface ownership or lifecycle mismatch"));
                }
                surface.revision = state.revision + 1;
                match operation.as_str() {
                    "updateComponents" => {
                        let mut seen = BTreeSet::new();
                        for component in payload["components"]
                            .as_array()
                            .ok_or_else(|| invalid("Missing components"))?
                        {
                            let id = component["id"]
                                .as_str()
                                .ok_or_else(|| invalid("Missing component ID"))?;
                            if !seen.insert(id) {
                                return Err(invalid("Duplicate component ID"));
                            }
                            surface
                                .snapshot
                                .components
                                .insert(id.to_owned(), component.clone());
                        }
                        validate_tree(&surface.snapshot.components)?;
                    }
                    "updateDataModel" => write_path(
                        &mut surface.snapshot.data_model,
                        payload["path"].as_str().unwrap_or("/"),
                        payload.get("value"),
                    )?,
                    "deleteSurface" => deleted = true,
                    _ => return Err(invalid("Unknown A2UI operation")),
                }
            }
            last = state.surfaces.get(surface_id).cloned();
            if deleted {
                state.surfaces.remove(surface_id);
            }
        }
        state.revision += 1;
        let event = json!({"revision":state.revision,"surface":last,"messages":input.messages,"deleted":deleted});
        state
            .publication_inputs
            .insert(invocation.to_owned(), raw_input);
        state
            .publications
            .insert(invocation.to_owned(), event.clone());
        self.save(session, version, &state).await?;
        Ok(event)
    }
    pub async fn admit_action(
        &self,
        session: &str,
        input: ActionInput,
    ) -> Result<ActionReceipt, AgentError> {
        if input.action_id.is_empty()
            || input.action_id.len() > 128
            || !safe(&input.message)
            || !safe(&input.metadata)
            || serde_json::to_string(&input)
                .map_err(|e| invalid(e.to_string()))?
                .len()
                > MAX_BYTES
        {
            return Err(invalid("Invalid A2UI action"));
        }
        let (version, mut state) = self.load(session).await?;
        if let Some(receipt) = state.actions.get(&input.action_id) {
            if serde_json::to_value(&receipt.input).ok() != serde_json::to_value(&input).ok() {
                return Err(StateError::Conflict.into());
            }
            return Ok(receipt.clone());
        }
        let validator = jsonschema::options()
            .should_validate_formats(true)
            .build(
                &serde_json::from_str::<Value>(include_str!("schemas/client_to_server.json"))
                    .map_err(|e| invalid(e.to_string()))?,
            )
            .map_err(|e| invalid(e.to_string()))?;
        if !validator.is_valid(&input.message) || input.message.get("action").is_none() {
            return Err(invalid("Invalid standard action envelope"));
        }
        if input.message["version"] != VERSION {
            return Err(invalid("Unsupported A2UI action version"));
        }
        let action = &input.message["action"];
        let id = action["surfaceId"]
            .as_str()
            .ok_or_else(|| invalid("Missing surface ID"))?;
        let surface = state.surfaces.get(id).ok_or(StateError::NotFound)?;
        if surface.mode != SurfaceMode::Reusable
            || surface.surface_instance_id != input.surface_instance_id
            || surface.revision != input.expected_revision
        {
            return Err(StateError::Conflict.into());
        }
        let component = surface
            .snapshot
            .components
            .get(action["sourceComponentId"].as_str().unwrap_or(""))
            .ok_or_else(|| invalid("Unknown action source"))?;
        if component.pointer("/action/event/name") != action.get("name")
            || !action["context"].is_object()
            || !action["timestamp"].is_string()
        {
            return Err(invalid("Action not declared by component"));
        }
        if let Some(models) = input
            .metadata
            .pointer("/a2uiClientDataModel/surfaces")
            .and_then(Value::as_object)
        {
            for id in models.keys() {
                if !state.surfaces.get(id).is_some_and(|s| {
                    s.owner_agent_id == surface.owner_agent_id && s.snapshot.send_data_model
                }) {
                    return Err(invalid("Client model belongs to another owner"));
                }
            }
        }
        let receipt = ActionReceipt {
            input: input.clone(),
            run_id: RunId::new_v4().to_string(),
            owner_agent_id: surface.owner_agent_id.clone(),
            claimed: false,
            completion: None,
        };
        // Canonical client model survives refresh even when the Agent updates only another path.
        if let Some(models) = input
            .metadata
            .pointer("/a2uiClientDataModel/surfaces")
            .and_then(Value::as_object)
        {
            if let Some(model) = models.get(id) {
                state.surfaces.get_mut(id).unwrap().snapshot.data_model = model.clone();
            }
        }
        // Admission changes the canonical surface, even if the model later emits
        // no UI update. Another page must not submit against the old revision.
        state.revision += 1;
        state.surfaces.get_mut(id).unwrap().revision = state.revision;
        state
            .actions
            .insert(input.action_id.clone(), receipt.clone());
        self.save(session, version, &state).await?;
        Ok(receipt)
    }
    /// Durable dispatch claim. An uncertain dispatch must be observed, never started again.
    pub async fn claim_action(&self, session: &str, id: &str) -> Result<bool, AgentError> {
        let (version, mut state) = self.load(session).await?;
        let receipt = state.actions.get_mut(id).ok_or(StateError::NotFound)?;
        if receipt.claimed {
            return Ok(false);
        }
        receipt.claimed = true;
        self.save(session, version, &state).await?;
        Ok(true)
    }
    pub async fn action_receipt(
        &self,
        session: &str,
        id: &str,
    ) -> Result<ActionReceipt, AgentError> {
        self.load(session)
            .await?
            .1
            .actions
            .get(id)
            .cloned()
            .ok_or_else(|| StateError::NotFound.into())
    }
    pub async fn complete_action(
        &self,
        session: &str,
        id: &str,
        completion: Value,
    ) -> Result<(), AgentError> {
        for _ in 0..8 {
            let (version, mut state) = self.load(session).await?;
            state
                .actions
                .get_mut(id)
                .ok_or(StateError::NotFound)?
                .completion = Some(completion.clone());
            match self.save(session, version, &state).await {
                Err(AgentError::State(StateError::Conflict)) => continue,
                result => return result,
            }
        }
        Err(StateError::Conflict.into())
    }
}
fn write_path(root: &mut Value, path: &str, value: Option<&Value>) -> Result<(), AgentError> {
    if path == "/" || path.is_empty() {
        *root = value.cloned().unwrap_or_else(|| json!({}));
        return Ok(());
    }
    if !path.starts_with('/')
        || path
            .as_bytes()
            .windows(2)
            .any(|p| p[0] == b'~' && ![b'0', b'1'].contains(&p[1]))
        || path.ends_with('~')
    {
        return Err(invalid("Invalid JSON Pointer"));
    }
    let parts = path[1..]
        .split('/')
        .map(|s| s.replace("~1", "/").replace("~0", "~"))
        .collect::<Vec<_>>();
    fn write(root: &mut Value, parts: &[String], value: Option<&Value>) -> Result<(), AgentError> {
        let key = &parts[0];
        if let Some(array) = root.as_array_mut() {
            let index = key
                .parse::<usize>()
                .map_err(|_| invalid("Invalid array index"))?;
            if index >= array.len() {
                return Err(invalid("Array index out of bounds"));
            }
            if parts.len() == 1 {
                if let Some(value) = value {
                    array[index] = value.clone();
                } else {
                    array.remove(index);
                }
            } else {
                write(&mut array[index], &parts[1..], value)?;
            }
        } else {
            if !root.is_object() {
                *root = json!({});
            }
            let map = root
                .as_object_mut()
                .ok_or_else(|| invalid("Invalid model"))?;
            if parts.len() == 1 {
                if let Some(value) = value {
                    map.insert(key.clone(), value.clone());
                } else {
                    map.remove(key);
                }
            } else {
                write(
                    map.entry(key.clone()).or_insert_with(|| json!({})),
                    &parts[1..],
                    value,
                )?;
            }
        }
        Ok(())
    }
    if parts
        .iter()
        .any(|p| ["__proto__", "constructor", "prototype"].contains(&p.as_str()))
    {
        return Err(invalid("Unsafe data model path"));
    }
    write(root, &parts, value)
}
fn validate_tree(components: &BTreeMap<String, Value>) -> Result<(), AgentError> {
    fn visit(
        id: &str,
        c: &BTreeMap<String, Value>,
        active: &mut BTreeSet<String>,
        done: &mut BTreeSet<String>,
    ) -> Result<(), AgentError> {
        if done.contains(id) {
            return Ok(());
        }
        if !active.insert(id.into()) {
            return Err(invalid("Component cycle"));
        }
        if let Some(v) = c.get(id) {
            let mut refs = Vec::new();
            for k in ["child", "trigger", "content"] {
                if let Some(id) = v[k].as_str() {
                    refs.push(id.to_owned());
                }
            }
            if let Some(a) = v["children"].as_array() {
                refs.extend(a.iter().filter_map(Value::as_str).map(str::to_owned));
            }
            if let Some(id) = v["children"]["componentId"].as_str() {
                refs.push(id.into());
            }
            if let Some(tabs) = v["tabs"].as_array() {
                refs.extend(
                    tabs.iter()
                        .filter_map(|t| t["child"].as_str())
                        .map(str::to_owned),
                );
            }
            for child in refs {
                visit(&child, c, active, done)?;
            }
        }
        active.remove(id);
        done.insert(id.into());
        Ok(())
    }
    let mut done = BTreeSet::new();
    for id in components.keys() {
        visit(id, components, &mut BTreeSet::new(), &mut done)?;
    }
    Ok(())
}
pub struct A2uiPlugin {
    service: A2uiService,
}
impl A2uiPlugin {
    pub fn new() -> Result<Self, AgentError> {
        Ok(Self {
            service: A2uiService::new()?,
        })
    }
}
#[async_trait]
impl Plugin for A2uiPlugin {
    fn id(&self) -> &str {
        "agent.a2ui"
    }
    async fn install(&self, ctx: &mut PluginContext<'_>) -> Result<(), PluginError> {
        ctx.provide(self.service.clone())?;
        let provider: Arc<dyn ToolProvider> = Arc::new(self.service.clone());
        ctx.contribute(ToolProviderService(provider.clone()))?;
        ctx.contribute_extension(provider)?;
        let guidance = Arc::new(guidance::Guidance);
        let context: Arc<dyn ContextContributor> = guidance.clone();
        let skills: Arc<dyn SkillProvider> = guidance;
        ctx.contribute(ContextContributorService(context.clone()))?;
        ctx.contribute_extension(context)?;
        ctx.contribute_extension(skills)
    }
    async fn ready(&self, ctx: &mut PluginReadyContext<'_>) -> Result<(), PluginError> {
        let store = ctx
            .services()
            .get::<SessionLifecycleStoreService>()
            .ok_or_else(|| PluginError::message("A2UI requires storage"))?;
        if let Some(interactions) = ctx.services().get::<InteractionStoreService>() {
            let _ = self.service.interactions.set(interactions.0.clone());
        }
        self.service
            .store
            .set(store.0.clone())
            .map_err(|_| PluginError::message("A2UI already initialized"))
    }
}
pub fn tool_descriptors() -> Vec<ToolDescriptor> {
    [false,true].into_iter().map(|input| ToolDescriptor::new(
        if input {"a2ui.request_input"} else {"a2ui.publish"},
        if input {"a2ui_request_input"} else {"a2ui_publish"},
        if input {"Publish a schema-validated SubmitOnce form and wait for its response to resume this run. Use the negotiated catalog and bind submit context.response to the response data model. Follow the a2ui-interface skill."} else {"Create, update or delete validated A2UI v0.9.1 surfaces. Choose Display for read-only reports or Reusable for repeated actions. Use the negotiated catalog, root component and complete envelopes; follow the a2ui-interface skill."},
        json!({"type":"object","additionalProperties":false,"required":["mode","messages"],"properties":{"mode":{"enum":if input{vec!["SubmitOnce"]}else{vec!["Display","Reusable"]}},"messages":{"type":"array","minItems":1,"maxItems":128,"items":{"type":"object"}},"responseSchema":{"type":"object"}}})
    )).collect()
}
#[async_trait]
impl ToolProvider for A2uiService {
    fn descriptors(&self) -> Option<Vec<ToolDescriptor>> {
        Some(
            tool_descriptors()
                .into_iter()
                .chain(std::iter::once(expand_descriptor()))
                .collect(),
        )
    }
    async fn lease(&self, run: &RunContext) -> Result<ToolLease, AgentError> {
        if !negotiated(&run.extensions) {
            return Ok(ToolLease::new(vec![]));
        }
        let Some(options) = run.extensions.get::<A2uiRunOptions>() else {
            return Ok(ToolLease::new(vec![]));
        };
        if options.capabilities.version != VERSION
            || !options
                .capabilities
                .catalog_ids
                .iter()
                .any(|id| self.validators.contains_key(id))
        {
            return Ok(ToolLease::new(vec![]));
        }
        Ok(ToolLease::new(
            tool_descriptors()
                .into_iter()
                .map(|descriptor| ToolBinding::new(descriptor, A2uiTool(self.clone())).hidden())
                .chain(std::iter::once(ToolBinding::new(
                    expand_descriptor(),
                    guidance::Expand,
                )))
                .collect(),
        ))
    }
}
struct A2uiTool(A2uiService);
#[async_trait]
impl Tool for A2uiTool {
    async fn call(&self, args: Value, run: &RunContext) -> Result<ToolOutput, ToolError> {
        let identity = run
            .extensions
            .get::<ToolCallIdentity>()
            .ok_or_else(|| ToolError::Failed("Missing tool identity".into()))?;
        let options = run
            .extensions
            .get::<A2uiRunOptions>()
            .ok_or_else(|| ToolError::Failed("A2UI not negotiated".into()))?;
        let result=async {
            if identity.tool_id=="a2ui.request_input"{
                let input:PublishInput=serde_json::from_value(args.clone()).map_err(|e|invalid(e.to_string()))?;
                if input.mode!=SurfaceMode::SubmitOnce{return Err(invalid("Expected SubmitOnce"));}
                let schema=input.response_schema.ok_or_else(||invalid("Missing responseSchema"))?;
                let mut request=InteractionRequest::input("Complete the form",schema)?;
                request.id=RunId::parse_str(&identity.invocation_id).map_err(|_|invalid("Invalid trusted tool invocation identity"))?;
                // Validate in an isolated ephemeral surface namespace; no durable duplicate form state.
                // A single create plus updates must be self-contained for durable resume.
                validate_input_messages(&self.0,&input.messages,&options.capabilities)?;
                request.presentation=Some(json!({"kind":"a2ui","messages":input.messages,"catalogIds":options.capabilities.catalog_ids}));
                let mut output=ToolOutput::text("Waiting for form response");output.structured=request.presentation.clone();return Ok(output.request_input(request));
            }
            let event=self.0.publish(&run.session_id,&options.owner_agent_id,&run.run_id.to_string(),&identity.invocation_id,&options.capabilities,args).await?;
            run.emit_output(OutputEvent::Custom{name:"agent.a2ui".into(),value:event.clone()});
            let mut output=ToolOutput::text("A2UI surface published");output.structured=Some(event);Ok(output)
        }.await;
        Ok(result.unwrap_or_else(|error| {
            let mut output = ToolOutput::error(error.to_string());
            output.structured =
                Some(json!({"error":{"code":"VALIDATION_FAILED","message":error.to_string()}}));
            output
        }))
    }
}
fn form_error_details(error: &jsonschema::ValidationError<'_>, index: usize) -> Vec<String> {
    use jsonschema::error::ValidationErrorKind;
    match error.kind() {
        ValidationErrorKind::OneOfNotValid { context }
        | ValidationErrorKind::AnyOf { context } => context.iter()
            .min_by_key(|branch| branch.len())
            .into_iter()
            .flatten()
            .flat_map(|child| form_error_details(child, index))
            .take(16)
            .collect(),
        _ => vec![format!("messages[{index}]{}: {error}", error.instance_path())],
    }
}

fn validate_input_messages(
    service: &A2uiService,
    messages: &[Value],
    capabilities: &A2uiClientCapabilities,
) -> Result<(), AgentError> {
    if messages.is_empty()
        || messages.len() > 128
        || serde_json::to_string(messages)
            .map_err(|e| invalid(e.to_string()))?
            .len()
            > MAX_BYTES
    {
        return Err(invalid("Invalid form size"));
    }
    let first = &messages[0]["createSurface"];
    let catalog = first["catalogId"]
        .as_str()
        .ok_or_else(|| invalid("Form must begin with createSurface"))?;
    let id = first["surfaceId"]
        .as_str()
        .ok_or_else(|| invalid("Missing surface ID"))?;
    if !capabilities.catalog_ids.iter().any(|c| c == catalog) {
        return Err(invalid("Catalog not negotiated"));
    }
    let validator = service
        .validators
        .get(catalog)
        .ok_or_else(|| invalid("Unsupported catalog"))?;
    let mut components = BTreeMap::new();
    for (index, message) in messages.iter().enumerate() {
        if !safe(message) {
            return Err(invalid(format!("messages[{index}]: unsafe form content")));
        }
        if message["version"] != VERSION {
            return Err(invalid(format!("messages[{index}]/version: expected {VERSION}")));
        }
        if !validator.is_valid(message) {
            let details = validator.iter_errors(message)
                .take(8)
                .flat_map(|error| form_error_details(&error, index))
                .collect::<Vec<_>>()
                .join("; ");
            return Err(invalid(format!("Invalid form envelope: {details}")));
        }
        let payload = message
            .as_object()
            .and_then(|m| m.iter().find(|(k, _)| k.as_str() != "version"))
            .map(|(_, v)| v)
            .ok_or_else(|| invalid("Missing form operation"))?;
        if payload["surfaceId"] != id
            || index > 0
                && (message.get("createSurface").is_some()
                    || message.get("deleteSurface").is_some())
        {
            return Err(invalid("One self-contained form surface required"));
        }
        if let Some(items) = message
            .pointer("/updateComponents/components")
            .and_then(Value::as_array)
        {
            for c in items {
                components.insert(c["id"].as_str().unwrap_or("").to_owned(), c.clone());
            }
        }
    }
    validate_tree(&components)?;
    if !components.contains_key("root") {
        return Err(invalid("Form root missing"));
    }
    Ok(())
}

#[cfg(test)]
mod form_validation_tests {
    use super::*;

    #[test]
    fn form_errors_locate_the_message_and_field() {
        let service = A2uiService::new().unwrap();
        let capabilities = A2uiClientCapabilities {
            version: VERSION.into(), catalog_ids: vec![AGENT_CATALOG_ID.into()],
        };
        let messages = vec![json!({"version": VERSION, "createSurface": {
            "surfaceId": "form", "catalogId": AGENT_CATALOG_ID, "sendDataModel": "wrong"
        }})];
        let error = validate_input_messages(&service, &messages, &capabilities).unwrap_err().to_string();
        assert!(error.contains("messages[0]"), "{error}");
        assert!(error.contains("sendDataModel"), "{error}");
        assert!(error.contains("boolean"), "{error}");
    }
}

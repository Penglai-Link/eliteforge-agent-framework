//! Shared runtime scenarios for framework transports and host regression tests.
use async_trait::async_trait;
use eliteforge_agent_framework::*;
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchOutcomeScenario {
    ParallelCancellation,
    ExposeFailure,
    SetExposureFailure,
    MixedApproval,
    ProcessorFailure,
}
impl BatchOutcomeScenario {
    pub fn name(self) -> &'static str {
        match self {
            Self::ParallelCancellation => "parallelCancellation",
            Self::ExposeFailure => "exposeFailure",
            Self::SetExposureFailure => "setExposureFailure",
            Self::MixedApproval => "mixedApproval",
            Self::ProcessorFailure => "processorFailure",
        }
    }
    pub fn binding(self) -> ToolBinding {
        let mut binding = ToolBinding::new(
            ToolDescriptor::new(
                "batch.outcome",
                "batch_outcome",
                "Outcome probe",
                json!({"type":"object"}),
            ),
            self,
        );
        binding.parallel_safe = matches!(
            self,
            Self::ParallelCancellation | Self::MixedApproval | Self::ProcessorFailure
        );
        binding
    }
}
pub struct BatchOutcomeModel;
#[async_trait]
impl ChatModel for BatchOutcomeModel {
    async fn generate(
        &self,
        _: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        if request.messages.iter().any(|m| m.role == Role::Tool) {
            return Ok(ChatResponse::text("done"));
        }
        Ok(ChatResponse::tool_calls(vec![ToolCall::new(
            "provider-batch",
            TOOL_EXECUTE_CALL_NAME,
            json!({"tool":"batch_outcome","arguments":{"items":[{"n":0},{"n":1},{"n":2}]}}),
        )]))
    }
}
#[async_trait]
impl Tool for BatchOutcomeScenario {
    async fn call(&self, args: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        match self {
            Self::ParallelCancellation if args["n"] != 1 => std::future::pending().await,
            Self::ParallelCancellation => Ok(ToolOutput::text("confirmed success")),
            Self::MixedApproval | Self::ProcessorFailure => {
                Ok(ToolOutput::text("confirmed success"))
            }
            Self::ExposeFailure => {
                Ok(ToolOutput::text("claimed success").expose_tools(vec!["not-authorized".into()]))
            }
            Self::SetExposureFailure => {
                let mut output = ToolOutput::text("claimed success");
                output.effect = ToolEffect::SetExposedTools(vec!["not-authorized".into()]);
                Ok(output)
            }
        }
    }
}
/// Cancels at the completion boundary, with an earlier sibling still in flight
/// and a later sibling queued. No timing sleeps or provider-specific behavior.
pub struct CancelAfterKnownResult;
#[async_trait]
impl ToolResultProcessor for CancelAfterKnownResult {
    async fn process(
        &self,
        invocation: &ToolInvocationContext<'_>,
        output: ToolOutput,
    ) -> Result<ToolOutput, AgentError> {
        if invocation.call.arguments["n"] == 1 && !output.is_error {
            invocation.run.cancellation.cancel();
        }
        Ok(output)
    }
}

/// A host audit/transform error must not prevent the native terminal commit.
#[derive(Clone, Default)]
pub struct FailSecondResult(pub std::sync::Arc<std::sync::Mutex<Vec<usize>>>);
#[async_trait]
impl ToolResultProcessor for FailSecondResult {
    async fn process(
        &self,
        invocation: &ToolInvocationContext<'_>,
        output: ToolOutput,
    ) -> Result<ToolOutput, AgentError> {
        let index = invocation.call.arguments["n"].as_u64().unwrap() as usize;
        self.0.lock().unwrap().push(index);
        if index == 1 {
            // This belongs to the host, not the framework checkpoint commit.
            return Err(StateError::Backend("host audit unavailable".into()).into());
        }
        Ok(output)
    }
}

/// Capture the actual next-turn request, including provider call/result pairing.
#[derive(Clone, Default)]
pub struct BatchRecoveryModel(pub std::sync::Arc<std::sync::Mutex<Vec<Message>>>);
#[async_trait]
impl ChatModel for BatchRecoveryModel {
    async fn generate(
        &self,
        model: &str,
        request: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        if request.messages.iter().any(|m| m.role == Role::Tool)
            || request
                .messages
                .iter()
                .any(|m| m.role == Role::User && m.content.text() == "continue")
        {
            *self.0.lock().unwrap() = request.messages;
            return Ok(ChatResponse::text("done"));
        }
        BatchOutcomeModel.generate(model, request, stream).await
    }
}
pub struct MixedBatchApproval;
#[async_trait]
impl ToolPolicy for MixedBatchApproval {
    async fn evaluate(
        &self,
        invocation: &ToolInvocationContext<'_>,
    ) -> Result<ToolPolicyDecision, AgentError> {
        Ok(if invocation.call.arguments["n"] == 0 {
            ToolPolicyDecision::Deny("item zero denied".into())
        } else {
            ToolPolicyDecision::RequireApproval("Approve item?".into())
        })
    }
}

/// Deterministic barriers let transports inspect committed progress while a
/// sibling is still running, without sleeps or racing instant tools.
#[derive(Clone)]
pub struct BatchProgressControl(pub std::sync::Arc<[tokio::sync::Semaphore; 3]>);
impl Default for BatchProgressControl {
    fn default() -> Self {
        Self(std::sync::Arc::new(std::array::from_fn(|_| {
            tokio::sync::Semaphore::new(0)
        })))
    }
}
impl BatchProgressControl {
    pub fn binding(&self) -> ToolBinding {
        let mut binding = ToolBinding::new(
            ToolDescriptor::new(
                "batch.outcome",
                "batch_outcome",
                "Controlled batch",
                json!({"type":"object"}),
            ),
            self.clone(),
        );
        binding.parallel_safe = true;
        binding
    }
    pub fn release(&self, index: usize) {
        self.0[index].add_permits(1);
    }
}
#[async_trait]
impl Tool for BatchProgressControl {
    async fn call(&self, args: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        self.0[args["n"].as_u64().unwrap() as usize]
            .acquire()
            .await
            .unwrap()
            .forget();
        Ok(ToolOutput::text("confirmed success"))
    }
}
pub struct BatchProgressApproval;
#[async_trait]
impl ToolPolicy for BatchProgressApproval {
    async fn evaluate(
        &self,
        invocation: &ToolInvocationContext<'_>,
    ) -> Result<ToolPolicyDecision, AgentError> {
        Ok(match invocation.call.arguments["n"].as_u64().unwrap() {
            0 => ToolPolicyDecision::Deny("item zero denied".into()),
            1 => ToolPolicyDecision::RequireApproval("Approve item?".into()),
            _ => ToolPolicyDecision::Allow,
        })
    }
}

pub fn batch_statuses(output: &Value) -> Vec<String> {
    output["structured"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["status"].as_str().unwrap().to_string())
        .collect()
}

pub fn committed_progress_snapshot(cp: &Checkpoint, snapshot: Value, expected: &[&str]) -> Value {
    let parent = cp
        .history
        .tool_invocations
        .iter()
        .find(|r| r.batch.is_none())
        .unwrap();
    assert_eq!(snapshot["toolCallId"], parent.id.to_string());
    assert_eq!(
        snapshot["revision"].as_str(),
        parent.batch_revision.as_deref()
    );
    assert_eq!(batch_statuses(&snapshot["output"]), expected);
    assert_eq!(
        Some(&snapshot["output"]["structured"]),
        parent.structured.as_ref()
    );
    recovery_snapshot(snapshot["output"].clone(), parent, Some(snapshot))
}

pub fn recovery_model_result(model: &BatchRecoveryModel) -> Value {
    let messages = model.0.lock().unwrap();
    let calls: Vec<_> = messages.iter().flat_map(|m| &m.tool_calls).collect();
    let results: Vec<_> = messages.iter().filter(|m| m.role == Role::Tool).collect();
    assert_eq!(calls.len(), 1, "original parent call appears exactly once");
    assert_eq!(results.len(), 1, "one paired provider result per parent");
    assert_eq!(
        results[0].tool_call_id.as_deref(),
        Some(calls[0].id.as_str())
    );
    serde_json::from_str(&results[0].content.text()).unwrap()
}

/// Cross-project snapshot including the exact generic activity payload.
pub fn recovery_snapshot(
    live: Value,
    parent: &HistoryToolInvocation,
    snapshot: Option<Value>,
) -> Value {
    let mut value = outcome_snapshot(live, parent);
    value["history"]["batchRevision"] = json!(parent.batch_revision);
    if let Some(snapshot) = snapshot {
        assert_eq!(
            snapshot["revision"].as_str(),
            parent.batch_revision.as_deref()
        );
        value["snapshot"] = snapshot;
        normalize_outcome_ids(&mut value, parent);
    }
    value
}

/// Strip clock fields and normalize random invocation UUIDs for committed wire
/// fixtures. Values still come from actual execution, never synthesized results.
pub fn outcome_snapshot(live: Value, parent: &HistoryToolInvocation) -> Value {
    let mut snapshot = json!({
        "live": live,
        "history": {"id":parent.id, "callId":parent.call_id, "toolName":parent.tool_name,
            "arguments":parent.arguments, "status":parent.status, "content":parent.content,
            "structured":parent.structured, "isError":parent.is_error},
    });
    normalize_outcome_ids(&mut snapshot, parent);
    snapshot
}
fn normalize_outcome_ids(snapshot: &mut Value, parent: &HistoryToolInvocation) {
    let batch: ToolBatchResult =
        serde_json::from_value(parent.structured.clone().unwrap()).unwrap();
    let mut identities = std::collections::BTreeMap::new();
    identities.insert(parent.id.to_string(), RunId::from_u128(1).to_string());
    for row in batch.results {
        identities.insert(
            row.identity.invocation_id.to_string(),
            RunId::from_u128(10 + row.identity.index as u128).to_string(),
        );
    }
    fn normalize(value: &mut Value, identities: &std::collections::BTreeMap<String, String>) {
        match value {
            Value::String(s) => {
                if let Some(id) = identities.get(s) {
                    *s = id.clone();
                }
            }
            Value::Array(values) => values.iter_mut().for_each(|v| normalize(v, identities)),
            Value::Object(values) => values.values_mut().for_each(|v| normalize(v, identities)),
            _ => {}
        }
    }
    normalize(snapshot, &identities);
}

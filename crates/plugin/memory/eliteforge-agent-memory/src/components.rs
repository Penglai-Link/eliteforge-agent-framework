use async_trait::async_trait;
use eliteforge_agent_core::*;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

/// Queries the current source for every assembly. Entries stay user-role data,
/// and each entry retains its origin and version. No automatic memory writes.
#[derive(Clone)]
pub struct MemoryContributor {
    id: String,
    namespace: String,
    memory: Arc<dyn LongTermMemory>,
    max_results: usize,
    max_bytes: usize,
}
impl MemoryContributor {
    pub fn new(
        id: impl Into<String>,
        namespace: impl Into<String>,
        memory: Arc<dyn LongTermMemory>,
    ) -> Result<Self, MemoryError> {
        let namespace = namespace.into();
        crate::long_term::validate_namespace(&namespace)?;
        let id = id.into();
        if id.trim().is_empty() {
            return Err(MemoryError::Invalid(
                "memory block ID cannot be empty".into(),
            ));
        }
        Ok(Self {
            id,
            namespace,
            memory,
            max_results: 20,
            max_bytes: 64 * 1024,
        })
    }
    pub fn with_read_limits(
        mut self,
        max_results: usize,
        max_bytes: usize,
    ) -> Result<Self, MemoryError> {
        if max_results == 0 || max_bytes == 0 {
            return Err(MemoryError::Invalid(
                "memory read limits must be positive".into(),
            ));
        }
        self.max_results = max_results;
        self.max_bytes = max_bytes;
        Ok(self)
    }
}
#[async_trait]
impl ContextContributor for MemoryContributor {
    fn block_id(&self) -> &str {
        &self.id
    }
    async fn contribute(&self, input: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
        input.run.check_cancelled()?;
        let mut messages = Vec::new();
        let mut versions = Vec::new();
        if input.budget.max_messages > 0 && input.budget.max_tokens > 0 {
            let query = MemoryQuery {
                text: input.current_input.text(),
                limit: self.max_results.min(input.budget.max_messages),
                max_bytes: self.max_bytes,
            };
            let entries = tokio::select! {
                biased;
                _ = input.run.cancelled() => return Err(AgentError::Cancelled),
                entries = self.memory.search(&self.namespace, &query) => entries?,
            };
            let mut remaining = input.budget.max_tokens;
            for entry in entries {
                let message = Message::user(Content(vec![ContentBlock::Json {
                    value: json!({"memory": entry}),
                }]));
                let tokens = input.estimator.message(input.model, &message).await?;
                if tokens <= remaining {
                    remaining -= tokens;
                    versions.push((entry.value.id, entry.version));
                    messages.push(message);
                }
            }
        }
        Ok(ContextBlock {
            id: self.id.clone(),
            messages,
            trust: ContextTrust::Data,
            source_version: Some(serde_json::to_string(&versions).expect("string pairs serialize")),
        })
    }
}

/// Read tools are enabled by default. Writes require explicit application opt-in;
/// their namespace is bound by the host and absent from model input schemas.
#[derive(Clone)]
pub struct MemoryTools {
    memory: Arc<dyn LongTermMemory>,
    namespace: String,
    allow_writes: bool,
}
impl MemoryTools {
    pub fn new(
        memory: Arc<dyn LongTermMemory>,
        namespace: impl Into<String>,
    ) -> Result<Self, MemoryError> {
        let namespace = namespace.into();
        crate::long_term::validate_namespace(&namespace)?;
        Ok(Self {
            memory,
            namespace,
            allow_writes: false,
        })
    }
    pub fn allow_writes(mut self) -> Self {
        self.allow_writes = true;
        self
    }
    pub fn bindings(&self) -> Vec<ToolBinding> {
        let mut operations = vec![Operation::Search, Operation::Get];
        if self.allow_writes {
            operations.extend([Operation::Save, Operation::Delete]);
        }
        operations
            .into_iter()
            .map(|operation| {
                let (name, description, schema) = operation.schema();
                let binding = ToolBinding::new(
                    ToolDescriptor::new(name, name, description, schema),
                    MemoryTool {
                        configured: self.clone(),
                        operation,
                    },
                );
                if operation.writes() {
                    binding
                } else {
                    binding.parallel_safe()
                }
            })
            .collect()
    }
}
#[async_trait]
impl ToolProvider for MemoryTools {
    async fn lease(&self, _: &RunContext) -> Result<ToolLease, AgentError> {
        Ok(ToolLease::new(self.bindings()))
    }
}
#[derive(Clone, Copy)]
enum Operation {
    Search,
    Get,
    Save,
    Delete,
}
impl Operation {
    fn writes(self) -> bool {
        matches!(self, Self::Save | Self::Delete)
    }
    fn schema(self) -> (&'static str, &'static str, Value) {
        let id = json!({"type":"string","minLength":1,"maxLength":512});
        let version = json!({"type":"string","minLength":1,"maxLength":128});
        let (name, description, properties, required) = match self {
            Self::Search => (
                "memory_search",
                "Search authorized long-term memories by all given keywords; returns current versions and origins.",
                json!({"query":{"type":"string","maxLength":2048}, "limit":{"type":"integer","minimum":1,"maximum":20}}),
                vec!["query"],
            ),
            Self::Get => (
                "memory_get",
                "Read a current memory entry by ID before correcting or deleting it.",
                json!({"id":id}),
                vec!["id"],
            ),
            Self::Save => (
                "memory_save",
                "Create a memory with a stable ID or correct it using expected_version. Supply an explicit source; this does not extract facts automatically.",
                json!({"id":id,"content":{"type":"string","minLength":1,"maxLength":16384},"source":{"type":"object","properties":{"id":{"type":"string","minLength":1,"maxLength":2048},"version":{"type":["string","null"]}},"required":["id"],"additionalProperties":false},"expected_version":version,"expires_at_ms":{"type":"integer","minimum":1}}),
                vec!["id", "content", "source"],
            ),
            Self::Delete => (
                "memory_delete",
                "Delete a memory only if expected_version still matches its current version.",
                json!({"id":id,"expected_version":version}),
                vec!["id", "expected_version"],
            ),
        };
        (
            name,
            description,
            json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
        )
    }
}
struct MemoryTool {
    configured: MemoryTools,
    operation: Operation,
}
#[async_trait]
impl Tool for MemoryTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        if context.is_cancelled() {
            return Err(ToolError::Failed(
                "memory tool cancelled before dispatch".into(),
            ));
        }
        let outcome = tokio::select! {
            biased;
            _ = context.cancelled() => return Err(if self.operation.writes() { ToolError::Indeterminate("memory write cancelled; verify the current entry before retrying".into()) } else { ToolError::Failed("memory read cancelled".into()) }),
            result = self.execute(arguments) => result,
        };
        match outcome {
            Ok(value) => Ok(ToolOutput {
                structured: Some(value),
                ..ToolOutput::text("Memory operation completed.")
            }),
            Err(error) => {
                let code = match error {
                    MemoryError::Indeterminate(message) => {
                        return Err(ToolError::Indeterminate(message));
                    }
                    MemoryError::NotFound => "memory_not_found",
                    MemoryError::Conflict => "memory_conflict",
                    MemoryError::Invalid(_) => "memory_invalid",
                    MemoryError::Capacity => "memory_capacity",
                    MemoryError::Backend(_) => "memory_backend",
                };
                if self.operation.writes() && matches!(error, MemoryError::Backend(_)) {
                    return Err(ToolError::Indeterminate(
                        "memory write outcome is unknown; inspect current state before retrying"
                            .into(),
                    ));
                }
                Ok(ToolOutput {
                    structured: Some(json!({"error":{"code":code}})),
                    ..ToolOutput::error(error.to_string())
                })
            }
        }
    }
}
impl MemoryTool {
    async fn execute(&self, arguments: Value) -> Result<Value, MemoryError> {
        let memory = &self.configured.memory;
        let namespace = &self.configured.namespace;
        let invalid = |_| MemoryError::Invalid("invalid memory tool arguments".into());
        match self.operation {
            Operation::Search => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Args {
                    query: String,
                    limit: Option<usize>,
                }
                let args: Args = serde_json::from_value(arguments).map_err(invalid)?;
                Ok(
                    json!({"entries":memory.search(namespace, &MemoryQuery { text: args.query, limit: args.limit.unwrap_or(20), max_bytes: 64 * 1024 }).await?}),
                )
            }
            Operation::Get => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Args {
                    id: String,
                }
                let args: Args = serde_json::from_value(arguments).map_err(invalid)?;
                Ok(json!({"entry":memory.get(namespace, &args.id).await?}))
            }
            Operation::Save => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Args {
                    id: String,
                    content: String,
                    source: MemorySource,
                    expected_version: Option<String>,
                    expires_at_ms: Option<u64>,
                }
                let args: Args = serde_json::from_value(arguments).map_err(invalid)?;
                let value = MemoryWrite {
                    id: args.id,
                    content: args.content,
                    source: args.source,
                    expires_at_ms: args.expires_at_ms,
                };
                Ok(
                    json!({"entry":memory.save(namespace, &value, args.expected_version.as_deref()).await?}),
                )
            }
            Operation::Delete => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Args {
                    id: String,
                    expected_version: String,
                }
                let args: Args = serde_json::from_value(arguments).map_err(invalid)?;
                memory
                    .delete(namespace, &args.id, &args.expected_version)
                    .await?;
                Ok(json!({"deleted":args.id}))
            }
        }
    }
}

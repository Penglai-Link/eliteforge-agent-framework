//! An offline SDK example: chat streaming, storage and nested middleware callbacks.
use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_memory::SqliteConversationStore;
use eliteforge_agent_state_sqlite::SqliteStateStore;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

struct DemoChat(String);
struct ToolResources;
#[async_trait]
impl BackendResource for ToolResources {
    async fn call(&self, _: &str, _: Value, _: BackendCallContext) -> Result<Value, RpcFault> {
        Err(RpcFault::new("unsupported tool resource method"))
    }
    async fn close(&self) -> Result<(), RpcFault> {
        Ok(())
    }
}
fn demo_skill() -> SkillSummary {
    SkillSummary {
        name: "demo-skill".into(),
        description: "Remote skill".into(),
        source: "demo".into(),
        location: "capability://demo-skill".into(),
        license: None,
        compatibility: None,
        metadata: Default::default(),
        allowed_tools: None,
    }
}
#[async_trait]
impl ChatModel for DemoChat {
    async fn generate(
        &self,
        _: &str,
        request: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        if request
            .messages
            .iter()
            .any(|message| message.content.text().contains("host-demo"))
        {
            let context = request
                .extensions
                .get::<BackendCallContext>()
                .ok_or_else(|| ModelError::Fatal("missing backend context".into()))?;
            let identity = context
                .host_call("host.context.identity", Value::Null)
                .await
                .map_err(|e| ModelError::Fatal(e.message))?;
            let echo=context.host_call("host.extension.invoke",json!({"implementation":"demo.custom","operation":"echo","params":"nested-model"})).await.map_err(|e|ModelError::Fatal(e.message))?;
            let trace = context
                .trace_context
                .as_ref()
                .ok_or_else(|| ModelError::Fatal("missing trace propagation".into()))?;
            return Ok(ChatResponse::text(format!(
                "{}:{}:{}",
                identity["session_id"].as_str().unwrap_or_default(),
                echo.as_str().unwrap_or_default(),
                trace.traceparent
            )));
        }
        if request
            .messages
            .iter()
            .any(|m| m.content.text().contains("skill-demo"))
        {
            let count = request
                .messages
                .iter()
                .filter(|m| m.role == Role::Tool)
                .count();
            let call = match count {
                0 => Some(("skill_load", json!({"name":"demo-skill"}))),
                1 => Some((
                    "skill_read_resource",
                    json!({"name":"demo-skill","path":"facts.txt"}),
                )),
                _ => None,
            };
            if let Some((name, arguments)) = call {
                return Ok(ChatResponse::tool_calls(vec![ToolCall {
                    invocation_id: RunId::nil(),
                    id: format!("skill-call-{count}"),
                    call_name: name.into(),
                    arguments,
                }]));
            }
        }
        if request
            .messages
            .iter()
            .any(|m| m.content.text().contains("tool-demo"))
            && !request.messages.iter().any(|m| m.role == Role::Tool)
        {
            let waiting = request
                .messages
                .iter()
                .any(|m| m.content.text().contains("tool-demo-wait"));
            return Ok(ChatResponse::tool_calls(vec![ToolCall {
                invocation_id: RunId::nil(),
                id: "provider-call".into(),
                call_name: "echo".into(),
                arguments: json!({"message":if waiting{"await-input"}else{"cross-process tool"}}),
            }]));
        }
        stream.text_delta(&self.0).await;
        Ok(ChatResponse::text(&self.0))
    }
}
#[tokio::main(worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = PathBuf::from(std::env::var("CAPABILITY_PACK_DIR")?);
    let manifest: CapabilityPackManifest =
        serde_json::from_slice(&std::fs::read(directory.join("manifest.json"))?)?;
    let selected: std::collections::BTreeSet<String> =
        serde_json::from_str(&std::env::var("CAPABILITY_SELECTION")?)?;
    let contributions: BackendContributions =
        serde_json::from_str(&std::env::var("CAPABILITY_CONTRIBUTIONS")?)?;
    // Selected plugin lifecycle can initialize its own backend services; an
    // unselected plugin must not acquire even a small external resource.
    if contributions.plugins.iter().any(|id| id == "demo.plugin") {
        std::fs::write("selected-plugin.marker", "ready")?;
    }
    let mut backend = TypedBackend::default();
    if selected.contains("demo.summaries") {
        let store = Arc::new(
            eliteforge_agent_context::SqliteSummaryStore::open("summary.db", Default::default())
                .await?,
        );
        backend.summary_store("demo.summaries", store.clone())?;
        backend.on_shutdown(move || async move {
            store
                .close()
                .await
                .map_err(|e| RpcFault::new(e.to_string()))
        });
    }
    if selected.contains("demo.artifacts") {
        std::fs::create_dir_all("artifacts")?;
        backend.artifact_store(
            "demo.artifacts",
            Arc::new(eliteforge_agent_context::FileArtifactStore::open(
                "artifacts",
                eliteforge_agent_context::FileArtifactLimits {
                    max_artifact_bytes: 32 * 1024 * 1024,
                    ..Default::default()
                },
            )?),
        )?;
    }

    let media = Arc::new(eliteforge_agent_capability_dev_host::DemoMedia);
    backend.embedding_model("demo.embedding", media.clone())?;
    backend.rerank_model("demo.rerank", media.clone())?;
    backend.image_model("demo.image", media.clone())?;
    backend.video_model("demo.video", media.clone())?;
    backend.speech_recognition_model("demo.recognition", media.clone())?;
    backend.speech_synthesis_model("demo.synthesis", media.clone())?;
    backend.realtime_model("demo.realtime", media)?;
    backend.chat_model(
        "demo.chat",
        Arc::new(DemoChat(format!("reply@{}", manifest.version))),
    )?;
    backend.token_estimator("demo.estimator", Arc::new(ConservativeTokenEstimator))?;
    if selected.contains("demo.memory") {
        backend.long_term_memory(
            "demo.memory",
            Arc::new(
                eliteforge_agent_memory::SqliteLongTermMemory::open(
                    "memory.db",
                    Default::default(),
                )
                .await?,
            ),
        )?;
    }
    backend.method("demo.discovery", "discover", |_: Value, _| async {
        Ok(vec![ModelDescriptor::new("offline")])
    })?;
    backend.method("demo.selection", "validate", |_: Value, _| async { Ok(()) })?;
    backend.method("demo.selection", "select", |_: Value, _| async {
        Ok(ModelRef::new("demo", "offline"))
    })?;
    backend.method("demo.context", "context", |_: Value, _| async {
        Ok(vec![Message::system("SDK-provided context")])
    })?;
    backend.method("demo.contributor", "contribute", |_: Value, _| async {
        Ok(ContextBlock::data("remote", "retrieved fact"))
    })?;
    backend.method("demo.reducer", "reduce", |params: Value, _| async move {
        let messages = params["block"]["messages"]
            .as_array()
            .ok_or_else(|| RpcFault::new("missing messages"))?;
        let limit = params["budget"]["max_messages"].as_u64().unwrap_or(1) as usize;
        Ok(messages
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>())
    })?;
    backend.method("demo.budget", "allocate", |params: Value, _| async move {
        let available: ContextBudget = serde_json::from_value(params["available"].clone())
            .map_err(|e| RpcFault::new(e.to_string()))?;
        let blocks = params["blocks"]
            .as_array()
            .ok_or_else(|| RpcFault::new("missing blocks"))?;
        Ok(blocks
            .iter()
            .map(|block| ContextBudget {
                max_messages: block["max_messages"]
                    .as_u64()
                    .map(|n| n as usize)
                    .unwrap_or(available.max_messages),
                max_tokens: block["max_tokens"]
                    .as_u64()
                    .map(|n| n as usize)
                    .unwrap_or(available.max_tokens / blocks.len().max(1)),
            })
            .collect::<Vec<_>>())
    })?;
    backend.method(
        "demo.assembler",
        "assemble",
        |params: Value, _| async move {
            let mut messages: Vec<Message> = serde_json::from_value(params["instructions"].clone())
                .map_err(|e| RpcFault::new(e.to_string()))?;
            messages.extend(
                serde_json::from_value::<Vec<Message>>(params["history"]["messages"].clone())
                    .map_err(|e| RpcFault::new(e.to_string()))?,
            );
            let report = ContextAssemblyReport {
                model: serde_json::from_value(params["model"].clone())
                    .map_err(|e| RpcFault::new(e.to_string()))?,
                stage: params["stage"].as_str().unwrap_or("plugin").into(),
                step: params["step"].as_u64().unwrap_or(0) as usize,
                attempt: params["attempt"].as_u64().unwrap_or(0) as usize,
                node_execution: None,
                input_capacity: params["budget"]["max_tokens"].as_u64().unwrap_or(1000) as usize,
                protocol_tokens: 0,
                estimated_input_tokens: 0,
                blocks: vec![],
            };
            Ok(ContextWindow { messages, report })
        },
    )?;
    backend.method("demo.model-middleware","intercept",|params:Value,ctx|async move{ctx.host_call("host.next",params["request"].clone()).await})?;
    backend.method("demo.node", "validate", |_: Value, _| async { Ok(()) })?;
    backend.method("demo.node", "execute", |params: Value, ctx| async move {
        let snapshot: ConversationSnapshot = serde_json::from_value(ctx.host_call("host.node.snapshot", Value::Null).await?).map_err(|e| RpcFault::new(e.to_string()))?;
        let output = params["input"]["config"].clone();
        ctx.host_call("host.node.save_with_conversation", json!({"checkpoint":{"output":output},"revision":snapshot.revision,"messages":[Message::assistant("remote node checkpoint")]})).await?;
        let updated: ConversationSnapshot = serde_json::from_value(ctx.host_call("host.node.snapshot", Value::Null).await?).map_err(|e| RpcFault::new(e.to_string()))?;
        if updated.revision <= snapshot.revision { return Err(RpcFault::new("conversation CAS did not advance")); }
        Ok(WorkflowNodeResult::completed(output))
    })?;
    backend.method("demo.node-middleware","intercept",|params:Value,ctx|async move{ctx.host_call("host.next",params["input"].clone()).await})?;
    for operation in ["on_started", "on_finished"] {
        backend.method("demo.lifecycle", operation, |_: Value, _| async { Ok(()) })?;
    }
    backend.method("demo.observation", "on_event", |_: Value, _| async {
        // The fixture deliberately never acknowledges observation. A run must
        // still finish, and closing the Agent must cancel this observer.
        std::future::pending::<()>().await;
        Ok(())
    })?;
    backend.method("demo.output", "output", |params: Value, _| async move {
        Ok(params["event"].clone())
    })?;
    backend.method("demo.tools","lease",|_:Value,ctx|async move{
        let handle=ctx.register_resource(Arc::new(ToolResources))?;
        Ok(json!({"handle":handle,"tools":[{"descriptor":{"id":"demo.echo","call_name":"echo","description":"Echo through the sidecar","input_schema":{"type":"object","properties":{"message":{"type":"string"}},"required":["message"]}},"exposed":true,"parallel_safe":true}]}))
    })?;
    backend.method("demo.tools", "call", |params: Value, _| async move {
        let text = params["arguments"]["message"].as_str().unwrap_or_default();
        if text == "await-input" {
            return Ok(ToolOutput::text("waiting").request_input(
                InteractionRequest::input("Your answer", json!({"type":"string"}))
                    .map_err(|e| RpcFault::new(e.to_string()))?,
            ));
        }
        Ok(ToolOutput::text(text))
    })?;
    backend.method("demo.tool-policy", "evaluate", |_: Value, _| async {
        Ok(ToolPolicyDecision::Allow)
    })?;
    backend.method(
        "demo.tool-middleware",
        "intercept",
        |_: Value, ctx| async move { ctx.host_call("host.next", Value::Null).await },
    )?;
    backend.method("demo.skills", "catalog", |_: Value, _| async {
        Ok(SkillCatalog {
            skills: vec![demo_skill()],
            diagnostics: vec![],
        })
    })?;
    backend.method("demo.skills", "load", |_: Value, _| async {
        Ok(SkillDetail {
            skill: demo_skill(),
            skill_markdown: "# Remote skill\nUse the echo tool.".into(),
            files: vec![SkillFile {
                path: "facts.txt".into(),
                size_bytes: 4,
            }],
        })
    })?;
    backend.method(
        "demo.skills",
        "read_resource",
        |params: Value, _| async move {
            Ok(SkillResource {
                skill_name: params["name"].as_str().unwrap_or_default().into(),
                path: params["path"].as_str().unwrap_or_default().into(),
                content: "fact".into(),
            })
        },
    )?;
    backend.method("demo.skills", "release", |_: Value, _| async { Ok(()) })?;
    if selected.contains("demo.state") || selected.contains("demo.interactions") {
        let state = Arc::new(SqliteStateStore::open("execution.db", Default::default()).await?);
        if selected.contains("demo.state") {
            backend.state_store("demo.state", state.clone())?;
        }
        if selected.contains("demo.interactions") {
            backend.interaction_store("demo.interactions", state)?;
        }
    }
    if selected.contains("demo.conversations") {
        backend.conversation_store(
            "demo.conversations",
            Arc::new(SqliteConversationStore::open("conversation.db", Default::default()).await?),
        )?;
    }
    backend.method("demo.middleware", "intercept", |_: Value, ctx| async move {
        let result = ctx.host_call("host.next", Value::Null).await?;
        // A continuation is scoped and single-use, including after a successful result.
        if ctx.host_call("host.next", Value::Null).await.is_ok() {
            return Err(RpcFault::new("continuation incorrectly reused"));
        }
        Ok(result)
    })?;
    backend.method("demo.loop", "validate", |_: Value, _| async { Ok(()) })?;
    backend.method("demo.loop","run",|params:Value,ctx|async move{
        if serde_json::from_value::<Content>(params["request"]["input"].clone()).is_ok_and(|input| input.text() == "nested-budget") {
            let request = json!({"implementation":"demo.custom","operation":"model_budget","params":null});
            ctx.host_call("host.extension.invoke", request.clone()).await?;
            if ctx.host_call("host.extension.invoke", request).await.is_ok() {
                return Err(RpcFault::new("nested scope reset model-call allowance"));
            }
            return Ok(RunResult { output: "quota preserved".into(), content: Content::from("quota preserved"), structured: None, interactions: vec![], status: RunStatus::Completed, usage: Default::default(), steps: 1, tool_calls: 0 });
        }
        if serde_json::from_value::<Content>(params["request"]["input"].clone()).is_ok_and(|input| input.text() == "remote-wait") || !params["resume"].is_null() {
            return remote_wait(params, ctx).await;
        }
        let nested=ctx.host_call("host.extension.invoke",json!({"implementation":"demo.custom","operation":"echo","params":{"nested":true}})).await?;
        if nested!=json!({"nested":true}){return Err(RpcFault::new("nested implementation returned invalid result"));}
        if ctx.host_call("host.extension.invoke",json!({"implementation":"demo.chat","operation":"generate","params":null})).await.is_ok(){return Err(RpcFault::new("undeclared implementation dependency was allowed"));}
        let response:ChatResponse=serde_json::from_value(ctx.host_call("host.model.invoke",json!({"model":ModelRef::new("demo","offline"),"request":ChatRequest{messages:vec![Message::user("nested")],tools:vec![],options:Default::default(),extensions:Default::default()},"stage":"plugin"})).await?).map_err(|e|RpcFault::new(e.to_string()))?;
        let _=params;
        Ok(RunResult{output:response.content.text(),content:response.content,structured:None,interactions:vec![],status:RunStatus::Completed,usage:response.usage,steps:1,tool_calls:0})
    })?;
    backend.method(
        "demo.custom",
        "echo",
        |value: Value, _| async move { Ok(value) },
    )?;
    backend.method("demo.custom", "model_budget", |_: Value, ctx| async move {
        ctx.host_call("host.model.invoke", json!({"model":ModelRef::new("demo","offline"),"request":ChatRequest{messages:vec![Message::user("budget")],tools:vec![],options:Default::default(),extensions:Default::default()},"stage":"plugin"})).await
    })?;
    backend.method("demo.custom", "crash", |_: Value, _| async move {
        let count = std::fs::read_to_string("unknown-write.count")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0)
            + 1;
        std::fs::write("unknown-write.count", count.to_string())
            .map_err(|e| RpcFault::new(e.to_string()))?;
        std::process::exit(23);
        #[allow(unreachable_code)]
        Ok(Value::Null)
    })?;
    backend.page_method("demo.page", "echo", |value: Value, ctx| async move {
        ctx.host_call(
            "host.page.publish",
            json!({"topic":"chart","data":{"source":"page-only","value":value}}),
        )
        .await?;
        Ok(value)
    })?;
    BackendServer::new(manifest, Arc::new(backend))?
        .serve()
        .await?;
    Ok(())
}

// Custom loops own their checkpoint schema and use host handles for atomic writes.
async fn remote_wait(params: Value, ctx: BackendCallContext) -> Result<RunResult, RpcFault> {
    let (mut checkpoint, request) = if params["resume"].is_null() {
        let request = InteractionRequest::input("Remote loop answer", json!({"type":"string"}))
            .map_err(|e| RpcFault::new(e.to_string()))?;
        let run_id = serde_json::from_value(params["run"]["run_id"].clone())
            .map_err(|e| RpcFault::new(e.to_string()))?;
        let session = params["run"]["session_id"]
            .as_str()
            .ok_or_else(|| RpcFault::new("missing session"))?;
        let checkpoint = Checkpoint::new(
            run_id,
            session,
            "remote-loop",
            1,
            json!({"interaction":request}),
        );
        let saved = ctx
            .host_call(
                "host.state.create",
                json!({"checkpoint":checkpoint,"previous_run":null}),
            )
            .await?;
        (
            serde_json::from_value::<Checkpoint>(saved)
                .map_err(|e| RpcFault::new(e.to_string()))?,
            request,
        )
    } else {
        let checkpoint: Checkpoint =
            serde_json::from_value(ctx.host_call("host.state.load", Value::Null).await?)
                .map_err(|e| RpcFault::new(e.to_string()))?;
        let request: InteractionRequest =
            serde_json::from_value(checkpoint.state["interaction"].clone())
                .map_err(|e| RpcFault::new(e.to_string()))?;
        (checkpoint, request)
    };
    let handle = ctx.host_call("host.state.claim", Value::Null).await?;
    let (status, output, interactions) = if params["resume"].is_null() {
        checkpoint.status = ExecutionStatus::Waiting;
        let record = InteractionRecord {
            session_id: checkpoint.session_id.clone(),
            run_id: checkpoint.run_id,
            node_invocation_id: None,
            request: request.clone(),
            tool_call: None,
            response: None,
            created_at_unix_ms: unix_time_ms(),
            answered_at_unix_ms: None,
        };
        ctx.host_call(
            "host.interaction.publish",
            serde_json::to_value(record).map_err(|e| RpcFault::new(e.to_string()))?,
        )
        .await?;
        (RunStatus::AwaitingInput, String::new(), vec![request])
    } else {
        let record: InteractionRecord = serde_json::from_value(
            ctx.host_call("host.interaction.get", json!({"id":request.id}))
                .await?,
        )
        .map_err(|e| RpcFault::new(e.to_string()))?;
        let response = record
            .response
            .ok_or_else(|| RpcFault::new("interaction has no persisted answer"))?;
        checkpoint.status = ExecutionStatus::Completed;
        (
            RunStatus::Completed,
            response.value.as_str().unwrap_or_default().to_owned(),
            vec![],
        )
    };
    ctx.host_call(
        "host.state.commit",
        json!({"handle":handle["handle"],"revision":checkpoint.revision,"next":checkpoint}),
    )
    .await?;
    ctx.host_call("host.state.release", handle).await?;
    Ok(RunResult {
        content: Content::from(output.clone()),
        output,
        structured: None,
        interactions,
        status,
        usage: Usage::default(),
        steps: 1,
        tool_calls: 0,
    })
}

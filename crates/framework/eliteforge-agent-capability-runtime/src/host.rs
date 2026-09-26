use crate::*;
use async_trait::async_trait;
use eliteforge_agent_core::*;
use eliteforge_agent_plugin_kernel::ResourceScope;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::BTreeMap;
fn decode<T: DeserializeOwned>(value: Value) -> Result<T, RpcFault> {
    serde_json::from_value(value).map_err(|e| RpcFault::new(e.to_string()))
}
fn encode<T: Serialize>(value: T) -> Result<Value, RpcFault> {
    serde_json::to_value(value).map_err(|e| RpcFault::new(e.to_string()))
}
fn fault(error: impl std::error::Error + 'static) -> RpcFault {
    let erased: &(dyn std::error::Error + 'static) = &error;
    let indeterminate = erased
        .downcast_ref::<AgentError>()
        .is_some_and(agent_indeterminate)
        || erased.downcast_ref::<ModelError>().is_some_and(|e| {
            matches!(
                e,
                ModelError::Indeterminate(_) | ModelError::State(StateError::Indeterminate(_))
            )
        })
        || erased
            .downcast_ref::<StateError>()
            .is_some_and(|e| matches!(e, StateError::Indeterminate(_)))
        || erased
            .downcast_ref::<ConversationError>()
            .is_some_and(|e| matches!(e, ConversationError::Indeterminate(_)))
        || erased
            .downcast_ref::<ToolError>()
            .is_some_and(|e| matches!(e, ToolError::Indeterminate(_)));
    if indeterminate {
        RpcFault::from(RuntimeError::Indeterminate(error.to_string()))
    } else {
        let data = erased
            .downcast_ref::<StateError>()
            .and_then(|e| serde_json::to_value(e).ok())
            .or_else(|| {
                erased
                    .downcast_ref::<ConversationError>()
                    .and_then(|e| serde_json::to_value(e).ok())
            })
            .or_else(|| {
                erased
                    .downcast_ref::<ModelError>()
                    .and_then(|e| serde_json::to_value(e).ok())
            })
            .or_else(|| {
                erased
                    .downcast_ref::<ToolError>()
                    .and_then(|e| serde_json::to_value(e).ok())
            });
        RpcFault {
            code: -32000,
            message: error.to_string(),
            data,
        }
    }
}
fn agent_indeterminate(error: &AgentError) -> bool {
    match error {
        AgentError::Indeterminate(_)
        | AgentError::State(StateError::Indeterminate(_))
        | AgentError::Model(
            ModelError::Indeterminate(_) | ModelError::State(StateError::Indeterminate(_)),
        )
        | AgentError::ConversationStore(ConversationError::Indeterminate(_))
        | AgentError::Context(ContextError::Indeterminate(_))
        | AgentError::LongTermMemory(MemoryError::Indeterminate(_)) => true,
        AgentError::ContextAssembly { source, .. } => agent_indeterminate(source),
        _ => false,
    }
}
/// Host retains the real RunContext and service references. The sidecar gets
/// only callback scope tokens and serializable request data.
pub(crate) struct RunHost<'a> {
    pub run: &'a RunContext,
    pub services: Option<&'a AgentServices>,
    pub models: Option<&'a dyn ChatExecutor>,
    pub estimator: Option<&'a dyn TokenEstimator>,
    pub node: Option<&'a WorkflowNodeContext>,
    pub permissions: &'a [String],
    caller: &'a str,
    conversation: Option<Box<dyn ConversationSession>>,
    tools: Vec<ToolBinding>,
    leases: BTreeMap<
        String,
        (
            ExecutionLease,
            std::sync::Arc<std::sync::atomic::AtomicBool>,
        ),
    >,
}
impl<'a> RunHost<'a> {
    pub fn new(
        run: &'a RunContext,
        services: Option<&'a AgentServices>,
        permissions: &'a [String],
        caller: &'a str,
    ) -> Self {
        Self {
            run,
            services,
            models: services.and_then(|s| s.models.as_deref()),
            estimator: None,
            node: None,
            permissions,
            caller,
            conversation: None,
            tools: Vec::new(),
            leases: BTreeMap::new(),
        }
    }
    async fn conversation(&mut self) -> Result<&mut Box<dyn ConversationSession>, RpcFault> {
        if self.conversation.is_none() {
            let services = self
                .services
                .ok_or_else(|| RpcFault::new("conversation service not in this scope"))?;
            self.conversation = Some(
                services
                    .conversations
                    .open(&self.run.session_id)
                    .await
                    .map_err(fault)?,
            );
        }
        Ok(self.conversation.as_mut().expect("opened conversation"))
    }
    pub async fn dispatch(&mut self, request: RpcRequest) -> Result<Value, RpcFault> {
        self.run.check_cancelled().map_err(fault)?;
        let method = request.method.as_str();
        let params = request.params;
        let capability = method
            .strip_prefix("host.")
            .ok_or_else(|| RpcFault::new("invalid host method"))?;
        if !self.permissions.iter().any(|permission| {
            permission == capability
                || permission
                    .strip_suffix(".*")
                    .is_some_and(|prefix| capability.starts_with(&format!("{prefix}.")))
        }) {
            return Err(RpcFault::new("host capability not declared"));
        }
        match method {
            "host.extension.invoke" => {
                let services = self
                    .services
                    .ok_or_else(|| RpcFault::new("extension directory not in this scope"))?;
                let directory = services
                    .extension_router
                    .as_ref()
                    .ok_or_else(|| RpcFault::new("extension directory unavailable"))?;
                let target = params["implementation"]
                    .as_str()
                    .ok_or_else(|| RpcFault::new("missing target implementation"))?;
                let operation = params["operation"]
                    .as_str()
                    .ok_or_else(|| RpcFault::new("missing target operation"))?;
                directory
                    .call(
                        self.caller,
                        target,
                        operation,
                        params["params"].clone(),
                        self.run,
                        services,
                    )
                    .await
                    .map_err(fault)
            }
            "host.output" => {
                self.run.emit_output(decode(params)?);
                Ok(Value::Null)
            }
            "host.agent.spawn_child" => {
                let runtime = self
                    .run
                    .extensions
                    .get::<AgentCollaborationService>()
                    .ok_or_else(|| RpcFault::new("collaboration runtime unavailable"))?;
                encode(
                    runtime
                        .0
                        .spawn_child(decode(params["request"].clone())?, self.run)
                        .await
                        .map_err(fault)?,
                )
            }
            "host.thread.create" => {
                let runtime = self
                    .run
                    .extensions
                    .get::<AgentCollaborationService>()
                    .ok_or_else(|| RpcFault::new("collaboration runtime unavailable"))?;
                encode(
                    runtime
                        .0
                        .create_thread(
                            decode(params["request"].clone())?,
                            decode(params["title"].clone())?,
                            self.run,
                        )
                        .await
                        .map_err(fault)?,
                )
            }
            "host.thread.wait" => {
                let runtime = self
                    .run
                    .extensions
                    .get::<AgentCollaborationService>()
                    .ok_or_else(|| RpcFault::new("collaboration runtime unavailable"))?;
                encode(
                    runtime
                        .0
                        .wait_thread(
                            params["thread_id"]
                                .as_str()
                                .ok_or_else(|| RpcFault::new("missing thread id"))?,
                            decode(params["run_id"].clone())?,
                            self.run,
                        )
                        .await
                        .map_err(fault)?,
                )
            }
            "host.thread.close" => {
                let runtime = self
                    .run
                    .extensions
                    .get::<AgentCollaborationService>()
                    .ok_or_else(|| RpcFault::new("collaboration runtime unavailable"))?;
                runtime
                    .0
                    .close_thread(
                        params["thread_id"]
                            .as_str()
                            .ok_or_else(|| RpcFault::new("missing thread id"))?,
                        self.run,
                    )
                    .await
                    .map_err(fault)?;
                Ok(Value::Null)
            }
            "host.context.identity" => Ok(
                json!({"run_id":self.run.run_id,"session_id":self.run.session_id,"execution_id":self.run.execution_id(),"usage":self.run.usage()}),
            ),
            "host.estimator.message" => {
                let estimator = self
                    .estimator
                    .ok_or_else(|| RpcFault::new("estimator not in this scope"))?;
                encode(
                    estimator
                        .message(
                            &decode(params["model"].clone())?,
                            &decode(params["message"].clone())?,
                        )
                        .await
                        .map_err(fault)?,
                )
            }
            "host.model.invoke" => {
                if !self
                    .run
                    .usage
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .admit_host_model_call(self.run.limits.max_steps)
                {
                    return Err(RpcFault::new("run model-call limit exceeded"));
                }
                let models = self
                    .models
                    .ok_or_else(|| RpcFault::new("model service not in this scope"))?;
                let model: ModelRef = decode(params["model"].clone())?;
                let mut request: ChatRequest = decode(params["request"].clone())?;
                request.extensions = self.run.extensions.clone();
                encode(
                    models
                        .invoke_auxiliary(
                            &model,
                            request,
                            params["stage"].as_str().unwrap_or("plugin"),
                            &self.run.limits,
                            self.run,
                        )
                        .await
                        .map_err(fault)?,
                )
            }
            "host.conversation.snapshot" => {
                encode(self.conversation().await?.snapshot().await.map_err(fault)?)
            }
            "host.conversation.append" => {
                let revision = params["revision"]
                    .as_u64()
                    .ok_or_else(|| RpcFault::new("missing revision"))?;
                let messages: Vec<Message> = decode(params["messages"].clone())?;
                self.conversation()
                    .await?
                    .append_at(revision, &messages)
                    .await
                    .map_err(fault)?;
                Ok(Value::Null)
            }
            "host.tools.list" => {
                let services = self
                    .services
                    .ok_or_else(|| RpcFault::new("tools not in this scope"))?;
                if self.tools.is_empty() {
                    for provider in &services.tools {
                        let lease = provider.lease(self.run).await.map_err(fault)?;
                        self.tools.extend(lease.tools);
                        self.run.own_resources(lease.resources);
                    }
                }
                encode(
                    self.tools
                        .iter()
                        .map(|binding| binding.descriptor.clone())
                        .collect::<Vec<_>>(),
                )
            }
            "host.tools.invoke" => {
                let services = self
                    .services
                    .ok_or_else(|| RpcFault::new("tools not in this scope"))?;
                let call: ToolCall = decode(params["call"].clone())?;
                let binding = self
                    .tools
                    .iter()
                    .find(|binding| {
                        binding.descriptor.call_name == call.call_name && binding.exposed
                    })
                    .cloned()
                    .ok_or_else(|| RpcFault::new("tool not exposed by this host scope"))?;
                let invocation = ToolInvocationContext {
                    invocation_id: &call.id,
                    call: &call,
                    descriptor: &binding.descriptor,
                    run: self.run,
                };
                for policy in &services.tool_policy {
                    match policy.evaluate(&invocation).await.map_err(fault)? {
                        ToolPolicyDecision::Allow => {}
                        ToolPolicyDecision::Deny(message) => return Err(RpcFault::new(message)),
                        ToolPolicyDecision::RequireApproval(prompt) => {
                            let manager = services.interactions.as_ref().ok_or_else(|| {
                                RpcFault::new("approval requires an InteractionStore")
                            })?;
                            if let Some(id) = params["approval_id"].as_str() {
                                let record = manager
                                    .0
                                    .get(&self.run.session_id, id.parse().map_err(fault)?)
                                    .await
                                    .map_err(fault)?;
                                if record.run_id != self.run.run_id
                                    || record.node_invocation_id
                                        != self.run.node_execution().map(|n| n.invocation_id)
                                    || record.tool_call.as_ref() != Some(&call)
                                    || !matches!(
                                        record.request.kind,
                                        InteractionKind::Approval { .. }
                                    )
                                    || record.response.as_ref().and_then(|r| r.value.as_bool())
                                        != Some(true)
                                {
                                    return Err(RpcFault::new(
                                        "approval does not authorize this tool call",
                                    ));
                                }
                            } else {
                                let request = InteractionRequest::approval(RunId::new_v4(), prompt);
                                manager
                                    .0
                                    .publish(&InteractionRecord {
                                        session_id: self.run.session_id.clone(),
                                        run_id: self.run.run_id,
                                        node_invocation_id: self
                                            .run
                                            .node_execution()
                                            .map(|n| n.invocation_id),
                                        request: request.clone(),
                                        tool_call: Some(call.clone()),
                                        response: None,
                                        created_at_unix_ms: unix_time_ms(),
                                        answered_at_unix_ms: None,
                                    })
                                    .await
                                    .map_err(fault)?;
                                return Err(RpcFault{code:-32010,message:"persist workflow state and return AwaitingInput before resuming this call".into(),data:Some(json!({"interaction":request}))});
                            }
                        }
                    }
                }
                if !self
                    .run
                    .usage
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .admit_host_tool_call(self.run.limits.max_tool_calls)
                {
                    return Err(RpcFault::new("run tool-call limit exceeded"));
                }
                encode(
                    services
                        .tool_invoker
                        .invoke(
                            &call.id,
                            &call,
                            &binding,
                            self.run,
                            self.run.limits.tool_timeout,
                        )
                        .await
                        .map_err(fault)?,
                )
            }
            "host.interaction.publish" => {
                let services = self
                    .services
                    .ok_or_else(|| RpcFault::new("interactions not in this scope"))?;
                let manager = services
                    .interactions
                    .as_ref()
                    .ok_or_else(|| RpcFault::new("interaction store unavailable"))?;
                let mut record: InteractionRecord = decode(params)?;
                record.session_id = self.run.session_id.clone();
                record.run_id = self.run.run_id;
                record.node_invocation_id = self.run.node_execution().map(|n| n.invocation_id);
                record.response = None;
                record.answered_at_unix_ms = None;
                manager.0.publish(&record).await.map_err(fault)?;
                encode(record.request)
            }
            "host.interaction.get" => {
                let manager = self
                    .services
                    .and_then(|s| s.interactions.as_ref())
                    .ok_or_else(|| RpcFault::new("interaction store unavailable"))?;
                let id: RunId = decode(params["id"].clone())?;
                let record = manager
                    .0
                    .get(&self.run.session_id, id)
                    .await
                    .map_err(fault)?;
                if record.run_id != self.run.run_id
                    || record.node_invocation_id
                        != self.run.node_execution().map(|n| n.invocation_id)
                {
                    return Err(RpcFault::new("interaction ownership mismatch"));
                }
                encode(record)
            }
            "host.node.snapshot" => encode(
                self.node
                    .ok_or_else(|| RpcFault::new("node journal not in this scope"))?
                    .conversation_snapshot()
                    .await
                    .map_err(fault)?,
            ),
            "host.node.save" => {
                self.node
                    .ok_or_else(|| RpcFault::new("node journal not in this scope"))?
                    .save(params)
                    .await
                    .map_err(fault)?;
                Ok(Value::Null)
            }
            "host.node.save_with_conversation" => {
                self.node
                    .ok_or_else(|| RpcFault::new("node journal not in this scope"))?
                    .save_with_conversation(
                        params["checkpoint"].clone(),
                        params["revision"]
                            .as_u64()
                            .ok_or_else(|| RpcFault::new("missing revision"))?,
                        &decode::<Vec<Message>>(params["messages"].clone())?,
                    )
                    .await
                    .map_err(fault)?;
                Ok(Value::Null)
            }
            "host.state.load" => {
                let state = self
                    .services
                    .and_then(|s| s.state.as_ref())
                    .ok_or_else(|| RpcFault::new("state not in this scope"))?;
                encode(state.load(self.run.run_id).await.map_err(fault)?)
            }
            "host.state.create" => {
                let state = self
                    .services
                    .and_then(|s| s.state.as_ref())
                    .ok_or_else(|| RpcFault::new("state not in this scope"))?;
                let mut checkpoint: Checkpoint = decode(params["checkpoint"].clone())?;
                checkpoint.run_id = self.run.run_id;
                checkpoint.session_id = self.run.session_id.clone();
                encode(
                    state
                        .create(checkpoint, decode(params["previous_run"].clone())?)
                        .await
                        .map_err(fault)?,
                )
            }
            "host.state.claim" => {
                let state = self
                    .services
                    .and_then(|s| s.state.as_ref())
                    .ok_or_else(|| RpcFault::new("state not in this scope"))?;
                let lease = state
                    .claim(
                        self.run.run_id,
                        self.run.execution_id(),
                        self.run.limits.run_timeout,
                    )
                    .await
                    .map_err(fault)?;
                let handle = uuid::Uuid::new_v4().to_string();
                let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let state = state.clone();
                let owned = lease.clone();
                let finished = released.clone();
                let mut resources = ResourceScope::default();
                let fallback_state = state.clone();
                let fallback_lease = owned.clone();
                let fallback_released = finished.clone();
                resources.on_cleanup(move || {
                    if !fallback_released.swap(true, std::sync::atomic::Ordering::AcqRel)
                        && let Ok(runtime) = tokio::runtime::Handle::try_current()
                    {
                        runtime.spawn(async move {
                            let _ = fallback_state.release(&fallback_lease).await;
                        });
                    }
                });
                resources.on_shutdown(move || async move {
                    if !finished.swap(true, std::sync::atomic::Ordering::AcqRel) {
                        state.release(&owned).await.map_err(|e| e.to_string())?;
                    }
                    Ok(())
                });
                self.run.own_resources(resources);
                self.leases.insert(handle.clone(), (lease, released));
                Ok(json!({"handle":handle}))
            }
            "host.state.commit" | "host.state.release" | "host.state.renew" => {
                let state = self
                    .services
                    .and_then(|s| s.state.as_ref())
                    .ok_or_else(|| RpcFault::new("state not in this scope"))?;
                let handle = params["handle"]
                    .as_str()
                    .ok_or_else(|| RpcFault::new("missing state lease handle"))?;
                let (lease, released) = self
                    .leases
                    .get(handle)
                    .ok_or_else(|| RpcFault::new("state lease handle not owned by this scope"))?;
                match method {
                    "host.state.release" => {
                        state.release(lease).await.map_err(fault)?;
                        released.store(true, std::sync::atomic::Ordering::Release);
                        self.leases.remove(handle);
                        Ok(Value::Null)
                    }
                    "host.state.renew" => {
                        let renewed = state
                            .renew(lease, self.run.limits.run_timeout)
                            .await
                            .map_err(fault)?;
                        let released = released.clone();
                        self.leases.insert(handle.into(), (renewed, released));
                        Ok(Value::Null)
                    }
                    _ => {
                        let next: Checkpoint = decode(params["next"].clone())?;
                        if next.run_id != self.run.run_id || next.session_id != self.run.session_id
                        {
                            return Err(RpcFault::new("checkpoint ownership mismatch"));
                        }
                        encode(
                            state
                                .commit(
                                    lease,
                                    params["revision"]
                                        .as_u64()
                                        .ok_or_else(|| RpcFault::new("missing revision"))?,
                                    next,
                                )
                                .await
                                .map_err(fault)?,
                        )
                    }
                }
            }
            _ => Err(RpcFault::new("host method unavailable in this scope")),
        }
    }
}
#[async_trait]
impl ScopedHost for RunHost<'_> {
    fn trace_context(&self) -> Option<TraceContext> {
        self.run
            .extensions
            .get::<TraceContext>()
            .map(|trace| (*trace).clone())
    }
    async fn call(&mut self, request: RpcRequest) -> Result<Value, RpcFault> {
        self.dispatch(request).await
    }
}

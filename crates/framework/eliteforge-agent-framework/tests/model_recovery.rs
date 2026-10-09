use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::*;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct SideEffect(Arc<AtomicUsize>);
#[async_trait]
impl Tool for SideEffect {
    async fn call(&self, _: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("effect done"))
    }
}

struct ToolThenUnavailable(Arc<Mutex<Vec<String>>>);
#[async_trait]
impl ChatModel for ToolThenUnavailable {
    async fn generate(
        &self,
        model: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.0.lock().unwrap().push(model.into());
        if !request
            .messages
            .iter()
            .any(|message| message.role == Role::Tool)
        {
            return Ok(ChatResponse::tool_calls(vec![ToolCall::new(
                "effect",
                "effect",
                json!({}),
            )]));
        }
        match model {
            "old" => Err(ModelError::Unavailable(
                "removed after the tool completed".into(),
            )),
            "good" => Ok(ChatResponse::text("done")),
            _ => panic!("incompatible candidate dispatched"),
        }
    }
}

#[tokio::test]
async fn recovery_never_replays_completed_tools_or_exceeds_step_dispatch_budget() {
    for limit in [None, Some(1)] {
        let effects = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut incompatible = ModelDescriptor::new("no-tools");
        incompatible.supports_tools = Some(false);
        let agent = TestBuilder::default()
            .plugin(ModelConnectionPlugin::new(ModelConnection::new(
                "test",
                Arc::new(ToolThenUnavailable(calls.clone())),
                vec![
                    ModelDescriptor::new("old"),
                    incompatible,
                    ModelDescriptor::new("good"),
                ],
            )))
            .plugin(ModelStrategyPlugin(Arc::new(FixedModelStrategy(
                ModelRef::new("test", "old"),
            ))))
            .plugin(ModelRecoveryPlugin(ModelRecoveryPolicy {
                candidates: vec![
                    ModelRef::new("test", "no-tools"),
                    ModelRef::new("test", "good"),
                ],
                allow_explicit_model_replacement: false,
            }))
            .plugin(ToolProviderPlugin::new(
                "effects",
                StaticToolProvider::new(vec![ToolBinding::new(
                    ToolDescriptor::new("effect", "effect", "effect", json!({"type":"object"})),
                    SideEffect(effects.clone()),
                )]),
            ))
            .build()
            .await
            .unwrap();
        let mut request = RunRequest::new("effects", "do it");
        request.limits.max_model_calls_per_step = limit;
        let result = agent.run(request).await;
        assert_eq!(effects.load(Ordering::SeqCst), 1);
        if limit.is_none() {
            assert_eq!(result.unwrap().output, "done");
            assert_eq!(*calls.lock().unwrap(), vec!["old", "old", "good"]);
        } else {
            assert!(result.unwrap_err().to_string().contains("budget"));
            assert_eq!(*calls.lock().unwrap(), vec!["old", "old"]);
        }
    }
}

struct PendingRefresh(Arc<tokio::sync::Notify>);
#[async_trait]
impl ModelCatalogRefresh for PendingRefresh {
    fn connection_id(&self) -> &str {
        "test"
    }
    fn account_identity(&self) -> Option<ModelAccountIdentity> {
        Some(ModelAccountIdentity {
            scope: "alice".into(),
            revision: 1,
        })
    }
    async fn refresh(&self) -> Result<ModelRuntimeUpdate, ModelError> {
        self.0.notify_one();
        std::future::pending().await
    }
}
struct PendingRefreshPlugin(Arc<tokio::sync::Notify>);
#[async_trait]
impl Plugin for PendingRefreshPlugin {
    fn id(&self) -> &str {
        "test.pending-refresh"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.contribute(ModelCatalogRefreshService(Arc::new(PendingRefresh(
            self.0.clone(),
        ))))
    }
}

#[tokio::test]
async fn recovery_refresh_obeys_cancellation_and_model_timeout() {
    for cancel in [false, true] {
        let reached = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let agent = TestBuilder::default()
            .plugin(ModelConnectionPlugin::new(ModelConnection::new(
                "test",
                Arc::new(Provider {
                    calls: calls.clone(),
                    category: ProviderErrorCategory::Unavailable,
                    partial: false,
                }),
                vec![ModelDescriptor::new("old"), ModelDescriptor::new("good")],
            )))
            .plugin(ModelStrategyPlugin(Arc::new(FixedModelStrategy(
                ModelRef::new("test", "old"),
            ))))
            .plugin(ModelRecoveryPlugin(ModelRecoveryPolicy {
                candidates: vec![ModelRef::new("test", "good")],
                ..Default::default()
            }))
            .plugin(PendingRefreshPlugin(reached.clone()))
            .build()
            .await
            .unwrap();
        let mut request = RunRequest::new("timeout", "hello");
        request.limits.model_timeout = std::time::Duration::from_millis(100);
        let handle = agent.start(request).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), reached.notified())
            .await
            .unwrap();
        if cancel {
            agent.cancel(&handle).unwrap();
        }
        let error = tokio::time::timeout(std::time::Duration::from_secs(2), agent.wait(handle))
            .await
            .unwrap()
            .unwrap_err();
        if cancel {
            assert!(matches!(error, AgentError::Cancelled));
        } else {
            assert!(error.to_string().contains("refresh timed out"));
        }
        assert_eq!(*calls.lock().unwrap(), vec!["old"]);
    }
}

struct Refresh {
    update: ModelRuntimeUpdate,
    calls: AtomicUsize,
    notifications: AtomicUsize,
    switch_account: bool,
}

#[async_trait]
impl ModelCatalogRefresh for Refresh {
    fn request_refresh(&self) {
        self.notifications.fetch_add(1, Ordering::SeqCst);
    }
    fn connection_id(&self) -> &str {
        "test"
    }
    fn account_identity(&self) -> Option<ModelAccountIdentity> {
        Some(ModelAccountIdentity {
            scope: if self.switch_account && self.calls.load(Ordering::SeqCst) > 0 {
                "bob"
            } else {
                "alice"
            }
            .into(),
            revision: 1,
        })
    }
    async fn refresh(&self) -> Result<ModelRuntimeUpdate, ModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.update.clone())
    }
}

struct RefreshPlugin(Arc<Refresh>);
#[async_trait]
impl Plugin for RefreshPlugin {
    fn id(&self) -> &str {
        "test.refresh"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.contribute(ModelCatalogRefreshService(self.0.clone()))
    }
}

#[tokio::test]
async fn recovery_reads_refreshed_bindings_but_rejects_an_account_switch() {
    for (switch_account, enabled, removed) in [
        (false, true, false),
        (true, true, false),
        (false, false, false),
        (false, false, true),
    ] {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(Provider {
            calls: calls.clone(),
            category: ProviderErrorCategory::Unavailable,
            partial: false,
        });
        let runtime = ModelRuntime::new(ModelRuntimeUpdate::new("", 0, vec![])).unwrap();
        runtime
            .replace_connection(
                "test",
                ModelRuntimeUpdate::new(
                    "alice",
                    1,
                    vec![ModelConnection::new(
                        "test",
                        provider.clone(),
                        vec![ModelDescriptor::new("old")],
                    )],
                ),
            )
            .unwrap();
        let refresh = Arc::new(Refresh {
            update: ModelRuntimeUpdate::new(
                "alice",
                1,
                vec![ModelConnection::new(
                    "test",
                    provider,
                    vec![ModelDescriptor::new("good")],
                )],
            ),
            calls: AtomicUsize::new(0),
            notifications: AtomicUsize::new(0),
            switch_account,
        });
        let agent = TestBuilder::default()
            .default_models(false)
            .plugin(ModelRuntimePlugin::new(runtime.clone()))
            .plugin(RefreshPlugin(refresh.clone()))
            .plugin(ModelStrategyPlugin(Arc::new(FixedModelStrategy(
                ModelRef::new("test", "old"),
            ))))
            .plugin(ModelRecoveryPlugin(ModelRecoveryPolicy {
                candidates: if enabled {
                    vec![ModelRef::new("test", "good")]
                } else {
                    vec![]
                },
                allow_explicit_model_replacement: false,
            }))
            .build()
            .await
            .unwrap();
        if removed {
            runtime
                .replace_connection("test", ModelRuntimeUpdate::new("alice", 1, vec![]))
                .unwrap();
        }
        let result = agent.run(RunRequest::new("recovery", "hello")).await;
        assert_eq!(
            refresh.calls.load(Ordering::SeqCst),
            usize::from(enabled),
            "{result:?}"
        );
        assert_eq!(refresh.notifications.load(Ordering::SeqCst), 1);
        if !enabled {
            assert_eq!(result.unwrap_err().kind(), "model_unavailable");
            assert_eq!(
                *calls.lock().unwrap(),
                if removed {
                    Vec::<String>::new()
                } else {
                    vec!["old".to_string()]
                }
            );
        } else if switch_account {
            assert_eq!(result.unwrap_err().kind(), "model_account_changed");
            assert_eq!(*calls.lock().unwrap(), vec!["old"]);
        } else {
            assert_eq!(result.unwrap().output, "recovered");
            assert_eq!(*calls.lock().unwrap(), vec!["old", "good"]);
        }
    }
}

struct Provider {
    calls: Arc<Mutex<Vec<String>>>,
    category: ProviderErrorCategory,
    partial: bool,
}

#[async_trait]
impl ChatModel for Provider {
    async fn generate(
        &self,
        model: &str,
        _: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.calls.lock().unwrap().push(model.into());
        if model == "good" {
            return Ok(ChatResponse::text("recovered"));
        }
        if self.partial {
            stream.reasoning_delta("already visible").await;
        }
        Err(ModelError::Provider(Box::new(ProviderModelError {
            authorization_attempt: None,
            provider: "test".into(),
            category: self.category,
            http_status: Some(404),
            business_code: None,
            error_type: Some("gateway_error".into()),
            model: Some(ModelRef::new("test", model)),
            message: "unavailable".into(),
        })))
    }
}

#[tokio::test]
async fn recovery_respects_host_permissions_streaming_errors_and_budgets() {
    for (candidates, explicit, allow_explicit, partial, category, retries, expected) in [
        (
            false,
            false,
            false,
            false,
            ProviderErrorCategory::Unavailable,
            2,
            false,
        ),
        (
            true,
            false,
            false,
            false,
            ProviderErrorCategory::Unavailable,
            2,
            true,
        ),
        (
            true,
            true,
            false,
            false,
            ProviderErrorCategory::Unavailable,
            2,
            false,
        ),
        (
            true,
            true,
            true,
            false,
            ProviderErrorCategory::Unavailable,
            2,
            true,
        ),
        (
            true,
            false,
            false,
            true,
            ProviderErrorCategory::Unavailable,
            2,
            false,
        ),
        (
            true,
            false,
            false,
            false,
            ProviderErrorCategory::Permission,
            2,
            false,
        ),
        (
            true,
            false,
            false,
            false,
            ProviderErrorCategory::Quota,
            2,
            false,
        ),
        (
            true,
            false,
            false,
            false,
            ProviderErrorCategory::Authentication,
            2,
            false,
        ),
        (
            true,
            false,
            false,
            false,
            ProviderErrorCategory::Unavailable,
            0,
            false,
        ),
    ] {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let agent = TestBuilder::default()
            .plugin(ModelConnectionPlugin::new(ModelConnection::new(
                "test",
                Arc::new(Provider {
                    calls: calls.clone(),
                    category,
                    partial,
                }),
                vec![ModelDescriptor::new("old"), ModelDescriptor::new("good")],
            )))
            .plugin(ModelStrategyPlugin(Arc::new(FixedModelStrategy(
                ModelRef::new("test", "old"),
            ))))
            .plugin(ModelRecoveryPlugin(ModelRecoveryPolicy {
                candidates: if candidates {
                    vec![ModelRef::new("test", "good")]
                } else {
                    vec![]
                },
                allow_explicit_model_replacement: allow_explicit,
            }))
            .build()
            .await
            .unwrap();
        let mut request = RunRequest::new("recovery", "hello");
        if explicit {
            request.model = Some(ModelRef::new("test", "old"));
        }
        request.limits.max_model_retries = retries;
        let result = agent.run(request).await;
        assert_eq!(
            result.is_ok(),
            expected,
            "{category:?} explicit={explicit} partial={partial}: {result:?}"
        );
        let actual = calls.lock().unwrap().clone();
        assert_eq!(
            actual,
            if expected {
                vec!["old", "good"]
            } else {
                vec!["old"]
            }
        );
    }
}

#[tokio::test]
async fn recovery_skips_missing_and_already_failed_candidates() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let agent = TestBuilder::default()
        .plugin(ModelConnectionPlugin::new(ModelConnection::new(
            "test",
            Arc::new(Provider {
                calls: calls.clone(),
                category: ProviderErrorCategory::Unavailable,
                partial: false,
            }),
            vec![
                ModelDescriptor::new("old"),
                ModelDescriptor::new("bad"),
                ModelDescriptor::new("good"),
            ],
        )))
        .plugin(ModelStrategyPlugin(Arc::new(FixedModelStrategy(
            ModelRef::new("test", "old"),
        ))))
        .plugin(ModelRecoveryPlugin(ModelRecoveryPolicy {
            candidates: ["missing", "old", "bad", "bad", "good"]
                .map(|id| ModelRef::new("test", id))
                .to_vec(),
            allow_explicit_model_replacement: false,
        }))
        .build()
        .await
        .unwrap();
    assert_eq!(
        agent
            .run(RunRequest::new("recovery", "hello"))
            .await
            .unwrap()
            .output,
        "recovered"
    );
    assert_eq!(*calls.lock().unwrap(), vec!["old", "bad", "good"]);
}

struct MixedRecovery(Arc<Mutex<Vec<String>>>);
#[async_trait]
impl ChatModel for MixedRecovery {
    async fn generate(
        &self,
        model: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.0.lock().unwrap().push(model.into());
        match model {
            "old" => Err(ModelError::Unavailable("model rejected".into())),
            "bad" => Err(ModelError::Transient("HTTP 500 before output".into())),
            "good" => Ok(ChatResponse::text("recovered")),
            _ => unreachable!(),
        }
    }
}
#[tokio::test]
async fn transient_replacement_failure_advances_without_expanding_dispatch_budget() {
    for limit in [2, 3] {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let agent = TestBuilder::default()
            .plugin(ModelConnectionPlugin::new(ModelConnection::new(
                "test",
                Arc::new(MixedRecovery(calls.clone())),
                ["old", "bad", "good"].map(ModelDescriptor::new).to_vec(),
            )))
            .plugin(ModelStrategyPlugin(Arc::new(FixedModelStrategy(
                ModelRef::new("test", "old"),
            ))))
            .plugin(ModelRecoveryPlugin(ModelRecoveryPolicy {
                candidates: ["old", "bad", "good"]
                    .map(|id| ModelRef::new("test", id))
                    .to_vec(),
                allow_explicit_model_replacement: false,
            }))
            .build()
            .await
            .unwrap();
        let mut request = RunRequest::new("mixed-recovery", "hello");
        request.limits.max_model_retries = 2;
        request.limits.max_model_calls_per_step = Some(limit);
        let result = agent.run(request).await;
        if limit == 3 {
            assert_eq!(result.unwrap().output, "recovered");
            assert_eq!(*calls.lock().unwrap(), vec!["old", "bad", "good"]);
        } else {
            assert!(result.unwrap_err().to_string().contains("budget"));
            assert_eq!(*calls.lock().unwrap(), vec!["old", "bad"]);
        }
    }
}

use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_memory::{CompressionOptions, MemoryCompactionResult, MemoryCompactor};
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
#[derive(Clone)]
struct Provider {
    mode: &'static str,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}
#[async_trait]
impl ChatModel for Provider {
    async fn generate(
        &self,
        _: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        let call_number = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request.clone());
            requests.len()
        };
        if self.mode == "delay" {
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        if self.mode == "retry_usage" && call_number == 1 {
            return Err(ModelError::Transient(
                "failed attempt has unknown usage".into(),
            ));
        }
        if self.mode == "failure" {
            return Err(ModelError::Fatal("fixture failure".into()));
        }
        assert!(request.tools.is_empty());
        assert_eq!(request.messages[0].role, Role::System);
        assert!(matches!(
            request.messages[1].content.0.first(),
            Some(ContentBlock::Json { .. })
        ));
        let mut response=ChatResponse{content:json!({"summary":if self.mode=="oversized"{"é".repeat(1_000)}else{"retained historical facts".into()}}).to_string().into(),usage:Usage{input_tokens:12,output_tokens:6},..Default::default()};
        if self.mode == "unknown_usage" || (self.mode == "mixed_usage" && call_number == 2) {
            response.usage = Usage::default();
        }
        if self.mode == "metrics" || self.mode == "mixed_metrics" {
            response = response.with_provider_metrics(ProviderMetrics {
                cached_input_tokens: if self.mode == "mixed_metrics" && call_number == 2 {
                    None
                } else {
                    Some(5)
                },
                reasoning_tokens: Some(2),
            });
        }
        if self.mode == "invalid" {
            response.content = "unstructured plain text".into();
        }
        if self.mode == "tools" {
            response
                .tool_calls
                .push(ToolCall::new("id", "tool", json!({})));
        }
        Ok(response)
    }
}
struct Workflow {
    options: CompressionOptions,
    retry: bool,
}
struct RetryTransient;
#[async_trait]
impl ModelMiddleware for RetryTransient {
    async fn intercept(
        &self,
        request: ChatRequest,
        context: &ModelInvocationContext<'_>,
        next: ModelNext<'_>,
    ) -> Result<ChatResponse, ModelError> {
        let retry = ModelNext { ..next };
        match next.run(request.clone(), context).await {
            Err(ModelError::Transient(_)) => retry.run(request, context).await,
            result => result,
        }
    }
}
#[async_trait]
impl ModelWorkflow for Workflow {
    async fn run(
        &self,
        run: &RunContext,
        models: &dyn ChatExecutor,
        limits: &RunLimits,
        model: &ModelRef,
    ) -> Result<ChatResponse, AgentError> {
        let history = vec!["older authorized data ".repeat(190); 3];
        let unchanged = history.clone();
        let retry_invoker = self.retry.then(|| {
            ModelInvoker::new(models.catalog().clone(), None)
                .unwrap()
                .with_middleware(vec![Arc::new(RetryTransient)])
        });
        let models = retry_invoker
            .as_ref()
            .map(|models| models as &dyn ChatExecutor)
            .unwrap_or(models);
        let result = MemoryCompactor::new(model.clone(), self.options.clone())?
            .compact_history_with_usage(
                &history,
                Some("previous derived summary"),
                run,
                models,
                limits,
            )
            .await?;
        assert_eq!(history, unchanged);
        Ok(result.into_chat_response())
    }
}
fn job(provider: Provider, calls: usize) -> ChatModelJob {
    ChatModelJob {
        workflow: Some(Arc::new(Workflow {
            retry: provider.mode == "retry_usage",
            options: CompressionOptions {
                max_input_tokens: 6_000,
                max_summary_bytes: 128,
                max_model_calls: calls,
            },
        })),
        connection: ModelConnection::new(
            "memory",
            Arc::new(provider),
            vec![ModelDescriptor::new("summary")],
        ),
        model: ModelRef::new("memory", "summary"),
        request: ChatRequest {
            messages: vec![],
            tools: vec![],
            extensions: Default::default(),
            options: Default::default(),
        },
        limits: RunLimits {
            max_model_retries: 0,
            max_context_tokens: 8_000,
            max_total_tokens: 80_000,
            max_model_output_tokens: 1_024,
            ..Default::default()
        },
        stage: "memory.compaction".into(),
        max_calls: 16,
        repair: None,
    }
}
#[tokio::test]
async fn compaction_segments_under_sdk_model_budget_and_preserves_source() {
    let provider = Provider {
        mode: "ok",
        requests: Default::default(),
    };
    let client = ModelJobClient::new().await.unwrap();
    let response = client.execute(job(provider.clone(), 16)).await.unwrap();
    assert_eq!(response.content.text(), "retained historical facts");
    let calls = provider.requests.lock().unwrap().len() as u64;
    assert!(calls >= 2);
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 12 * calls,
            output_tokens: 6 * calls
        }
    );
    let result = MemoryCompactionResult::from_chat_response(response).unwrap();
    assert_eq!(result.provider_usage, Some(result.usage));
    client.close().await;
}
#[tokio::test]
async fn compaction_unknown_or_mixed_provider_usage_keeps_charge_without_actual_audit() {
    for mode in ["unknown_usage", "mixed_usage"] {
        let provider = Provider {
            mode,
            requests: Default::default(),
        };
        let client = ModelJobClient::new().await.unwrap();
        let response = client.execute(job(provider.clone(), 16)).await.unwrap();
        let result = MemoryCompactionResult::from_chat_response(response).unwrap();
        let calls = provider.requests.lock().unwrap().len() as u64;
        assert!(calls >= 2, "test must cover segmented calls");
        assert_eq!(result.summary, "retained historical facts");
        assert_eq!(result.provider_usage, None);
        assert!(result.usage.input_tokens > 12 * calls);
        assert!(result.usage.output_tokens > 6 * calls);
        client.close().await;
    }
}
#[test]
fn memory_usage_bridge_missing_or_invalid_metadata_never_fabricates_provider_usage() {
    let result = MemoryCompactionResult::from_chat_response(ChatResponse {
        content: "checkpoint".into(),
        usage: Usage {
            input_tokens: 80,
            output_tokens: 10,
        },
        ..Default::default()
    })
    .unwrap();
    assert_eq!(result.provider_usage, None);
    assert_eq!(result.usage.input_tokens, 80);
    assert!(
        MemoryCompactionResult::from_chat_response(ChatResponse {
            structured: Some(json!({"memory_compaction_provider_usage": "invalid"})),
            ..Default::default()
        })
        .is_err()
    );
}
#[tokio::test]
async fn compaction_invalid_schema_tools_oversize_failure_and_call_limit_fail_closed() {
    for mode in ["invalid", "tools", "oversized", "failure", "ok"] {
        let provider = Provider {
            mode,
            requests: Default::default(),
        };
        let client = ModelJobClient::new().await.unwrap();
        assert!(
            client
                .execute(job(provider.clone(), if mode == "ok" { 1 } else { 16 }))
                .await
                .is_err(),
            "mode {mode}"
        );
        client.close().await;
    }
}
#[tokio::test]
async fn compaction_cancellation_timeout_and_close_drain_inflight_calls() {
    for mode in 0..3 {
        let provider = Provider {
            mode: "delay",
            requests: Default::default(),
        };
        let client = ModelJobClient::new().await.unwrap();
        let mut input = job(provider.clone(), 16);
        if mode == 1 {
            input.limits.model_timeout = Duration::from_millis(40);
        }
        let mut handle = client.start(input).unwrap();
        let control = handle.control().unwrap();
        let waiting_client = client.clone();
        let waiting = tokio::spawn(async move { waiting_client.wait(&mut handle).await });
        tokio::time::timeout(Duration::from_secs(3), async {
            while provider.requests.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if mode == 0 {
            control.cancel().unwrap();
        }
        if mode == 2 {
            client.close().await;
        }
        assert!(
            tokio::time::timeout(Duration::from_secs(3), waiting)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        client.close().await;
    }
}

#[tokio::test]
async fn compaction_internal_retry_unknown_attempt_marks_successful_usage_as_incomplete() {
    let provider = Provider {
        mode: "retry_usage",
        requests: Default::default(),
    };
    let client = ModelJobClient::new().await.unwrap();
    let mut input = job(provider.clone(), 16);
    input.limits.max_model_retries = 1;
    let result =
        MemoryCompactionResult::from_chat_response(client.execute(input).await.unwrap()).unwrap();
    let attempts = provider.requests.lock().unwrap().len() as u64;
    assert!(attempts >= 3);
    assert_eq!(result.summary, "retained historical facts");
    assert_eq!(result.provider_usage, None);
    assert!(result.usage.input_tokens > 12 * (attempts - 1));
    assert!(result.usage.output_tokens > 6 * (attempts - 1));
    client.close().await;
}

#[tokio::test]
async fn segmented_provider_cache_metrics_aggregate_and_missing_field_stays_unknown() {
    for mode in ["metrics", "mixed_metrics"] {
        let provider = Provider {
            mode,
            requests: Default::default(),
        };
        let client = ModelJobClient::new().await.unwrap();
        let result = MemoryCompactionResult::from_chat_response(
            client.execute(job(provider.clone(), 16)).await.unwrap(),
        )
        .unwrap();
        let calls = provider.requests.lock().unwrap().len() as u64;
        assert!(calls >= 2);
        assert_eq!(result.provider_usage, Some(result.usage));
        assert_eq!(
            result.provider_metrics,
            Some(ProviderMetrics {
                cached_input_tokens: if mode == "metrics" {
                    Some(5 * calls)
                } else {
                    None
                },
                reasoning_tokens: Some(2 * calls)
            })
        );
        client.close().await;
    }
}

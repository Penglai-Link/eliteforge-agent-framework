use async_trait::async_trait;
use eliteforge_agent_context::*;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_storage_local::{SqliteSummaryStore, SummaryStoreOptions};
use eliteforge_agent_test_support::*;
use serde_json::json;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Summaries {
    drafts: Vec<String>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}
#[async_trait]
impl ChatModel for Summaries {
    async fn generate(
        &self,
        _: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        let mut requests = self.requests.lock().unwrap();
        let index = requests.len();
        requests.push(request);
        if self.drafts[index.min(self.drafts.len() - 1)] == "invalid-json" {
            return Ok(ChatResponse::text("not valid JSON"));
        }
        Ok(ChatResponse {
            structured: Some(json!({"summary":self.drafts[index.min(self.drafts.len()-1)]})),
            ..Default::default()
        })
    }
}
struct EmptySummary;
#[async_trait]
impl ContextContributor for EmptySummary {
    fn block_id(&self) -> &str {
        "summary"
    }
    async fn contribute(&self, _: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
        Ok(ContextBlock {
            id: "summary".into(),
            messages: vec![],
            trust: ContextTrust::Data,
            source_version: None,
        })
    }
}
struct Split {
    directory: std::path::PathBuf,
    summary_tokens: usize,
    cached: bool,
    pressure: bool,
}
#[async_trait]
impl AgentLoop for Split {
    async fn run(
        &self,
        request: &RunRequest,
        run: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let summaries = Arc::new(
            SqliteSummaryStore::open(
                self.directory.join("summaries.sqlite"),
                SummaryStoreOptions::default(),
            )
            .await
            .unwrap(),
        );
        let model = ModelRef::new("test", "summary");
        // This intentionally tiny legacy byte option must not cap split summaries.
        let reducer = ModelCompressor::new(
            model.clone(),
            CompressionOptions {
                max_input_tokens: 256_000,
                max_summary_bytes: 16,
                ..Default::default()
            },
        )?
        .with_store(summaries, "split-tests")?;
        let mut signed = Message::assistant("native continuation");
        signed.tool_calls = vec![ToolCall::new("call", "inspect", json!({"path":"a"}))];
        signed.model_context = Some(ModelMessageContext {
            protocol: "custom".into(),
            model: model.clone(),
            format: "signed".into(),
            data: json!({"signature":"preserve-exactly"}),
        });
        let mut tool = Message::tool("call", "recent result");
        tool.tool_result = Some(ToolResultMetadata {
            is_error: false,
            structured: Some(json!({"kept":true})),
            result_reference: None,
        });
        let messages = vec![
            Message::user("项目代号青松；准确事实，完整旧历史。".repeat(1200)),
            Message::assistant("old answer"),
            Message::user("recent question"),
            signed,
            tool,
            Message::assistant("recent answer"),
            Message::user("/compact"),
            Message::user("/compact"),
        ];
        let original = messages.clone();
        let mut history = ConversationSnapshot::new(
            ConversationKey::new(&request.session_id),
            "generation",
            messages,
        );
        history.revision = 7;
        let assembler = DefaultContextAssembler::new(
            ContextLayout(vec![
                ContextBlockConfig::new(INSTRUCTIONS_BLOCK, BlockBudget::tokens(100)),
                if self.pressure {
                    ContextBlockConfig::new("summary", BlockBudget::tokens(self.summary_tokens))
                        .compress_at(3000, 2000)
                } else {
                    ContextBlockConfig::new("summary", BlockBudget::tokens(self.summary_tokens))
                },
                ContextBlockConfig::new(HISTORY_BLOCK, BlockBudget::tokens(500))
                    .reduce_with(reducer)
                    .compress_at(10_000, 2000),
            ]),
            vec![Arc::new(EmptySummary)],
        )?
        .with_history_summary_block("summary", 2)?
        .with_global_compression(Some(8000), !self.cached)?;
        let result = assembler
            .assemble(&ContextAssemblyInput {
                instructions: &[],
                history: &history,
                model: &model,
                estimator: &UnicodeTokenEstimator,
                run,
                models: services.models.as_deref().unwrap(),
                limits: &request.limits,
                budget: ContextBudget {
                    max_tokens: self.summary_tokens + 600,
                    max_messages: 100,
                },
                tools: &[],
                options: &GenerationOptions::default(),
                overflow_recoveries: 0,
                stage: "test",
                step: 1,
                attempt: 1,
            })
            .await?;
        assert_eq!(history.messages, original, "durable source must not change");
        let summary = result
            .report
            .blocks
            .iter()
            .find(|b| b.id == "summary")
            .unwrap();
        let recent = result
            .report
            .blocks
            .iter()
            .find(|b| b.id == HISTORY_BLOCK)
            .unwrap();
        assert_eq!(summary.final_message_count, 1);
        assert!(summary.final_tokens <= self.summary_tokens);
        assert_eq!(summary.configured_tokens, Some(self.summary_tokens));
        assert_eq!(recent.final_message_count, original.len() - 2);
        assert_eq!(
            &result.messages[1..],
            &original[2..],
            "all recent native fields survive exactly"
        );
        assert!(result.messages[0].content.text().contains("青松"));
        assert!(result.report.estimated_input_tokens <= result.report.input_capacity);
        Ok(RunResult {
            output: result.messages[0].content.text(),
            content: Content::default(),
            structured: None,
            interactions: vec![],
            status: RunStatus::Completed,
            usage: run.usage(),
            steps: 0,
            tool_calls: 0,
            external_session: None,
        })
    }
}
async fn run_split(
    directory: &std::path::Path,
    tokens: usize,
    drafts: Vec<String>,
    cached: bool,
    pressure: bool,
) -> (Result<RunResult, AgentError>, Vec<ChatRequest>) {
    let provider = Summaries {
        drafts,
        requests: Arc::default(),
    };
    let agent = TestBuilder::default()
        .plugin(ModelConnectionPlugin::new(ModelConnection::new(
            "test",
            Arc::new(provider.clone()),
            vec![ModelDescriptor::new("summary")],
        )))
        .plugin(AgentLoopPlugin::new(Split {
            directory: directory.into(),
            summary_tokens: tokens,
            cached,
            pressure,
        }))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("same-session", "unused");
    request.limits.max_model_output_tokens = 16_000;
    request.limits.max_context_tokens = 256_000;
    let result = agent.run(request).await;
    agent.framework().close().await;
    let requests = provider.requests.lock().unwrap().clone();
    (result, requests)
}
#[tokio::test]
async fn summary_has_own_budget_and_recent_signed_tool_turns_remain_raw_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let (first, requests) = run_split(
        dir.path(),
        200,
        vec!["项目代号青松，保留明确事实。".into()],
        false,
        false,
    )
    .await;
    first.unwrap();
    assert_eq!(requests.len(), 1);
    let (cached, requests) = run_split(
        dir.path(),
        200,
        vec!["must not be called".into()],
        true,
        false,
    )
    .await;
    cached.unwrap();
    assert!(
        requests.is_empty(),
        "saved split projection must survive a fresh framework and SQLite connection"
    );
}
#[tokio::test]
async fn overlong_chinese_summary_retries_against_summary_quota_independent_of_history_quota() {
    let dir = tempfile::tempdir().unwrap();
    let (result, requests) = run_split(
        dir.path(),
        200,
        vec!["青松".repeat(200), "项目代号青松".into()],
        false,
        false,
    )
    .await;
    result.unwrap();
    assert_eq!(requests.len(), 2);
    let parameters = |r: &ChatRequest| match &r.messages[1].content.0[0] {
        ContentBlock::Json { value } => value.clone(),
        _ => panic!(),
    };
    assert!(
        parameters(&requests[1])["overlong_draft"]["actual_tokens"]
            .as_u64()
            .unwrap()
            > parameters(&requests[0])["summary_max_tokens"]
                .as_u64()
                .unwrap()
    );
}
#[tokio::test]
async fn large_configured_summary_can_exceed_sixteen_kib_without_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let summary = format!("项目代号青松。{}", "事实甲；".repeat(1500));
    assert!(summary.len() > 16 * 1024);
    let (result, requests) =
        run_split(dir.path(), 16_000, vec![summary.clone()], false, false).await;
    assert!(result.unwrap().output.ends_with(&summary));
    assert_eq!(requests.len(), 1);
}
#[tokio::test]
async fn repeated_oversize_drafts_fail_without_publishing_an_invalid_summary() {
    let dir = tempfile::tempdir().unwrap();
    let (failed, requests) =
        run_split(dir.path(), 200, vec!["青松".repeat(200)], false, false).await;
    assert!(failed.is_err());
    assert_eq!(requests.len(), 3);
    let (result, requests) =
        run_split(dir.path(), 200, vec!["项目代号青松".into()], true, false).await;
    // With no valid persisted projection and no pressure this fixture's raw
    // history exceeds its quota, so the coordinator must generate a fresh draft.
    result.unwrap();
    assert_eq!(requests.len(), 1);
}

#[tokio::test]
async fn summary_block_pressure_recompresses_without_waiting_for_history_or_global_pressure() {
    let dir = tempfile::tempdir().unwrap();
    let (first, requests) = run_split(
        dir.path(),
        1000,
        vec!["项目代号青松；".repeat(30)],
        false,
        true,
    )
    .await;
    first.unwrap();
    assert_eq!(requests.len(), 1);
    let (next, requests) =
        run_split(dir.path(), 1000, vec!["项目代号青松".into()], true, true).await;
    next.unwrap();
    assert_eq!(
        requests.len(),
        1,
        "summary block threshold must trigger even with a small recent tail"
    );
    let params = match &requests[0].messages[1].content.0[0] {
        ContentBlock::Json { value } => value,
        _ => panic!(),
    };
    assert!(params["summary_max_tokens"].as_u64().unwrap() < 200);
    let (next, requests) = run_split(
        dir.path(),
        1000,
        vec!["must not be called".into()],
        true,
        true,
    )
    .await;
    next.unwrap();
    assert!(
        requests.is_empty(),
        "recompressed summary must fit and remain reusable"
    );
}

#[tokio::test]
async fn malformed_structured_summary_is_regenerated_with_strict_schema_and_bounded_attempts() {
    let dir = tempfile::tempdir().unwrap();
    let (result, requests) = run_split(
        dir.path(),
        200,
        vec!["invalid-json".into(), "项目代号青松".into()],
        false,
        false,
    )
    .await;
    result.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.options.output_schema.is_some())
    );
    let parameters = match &requests[1].messages[1].content.0[0] {
        ContentBlock::Json { value } => value,
        _ => panic!(),
    };
    assert!(parameters["overlong_draft"]["format_error"].is_string());
    let dir = tempfile::tempdir().unwrap();
    let (failed, requests) =
        run_split(dir.path(), 200, vec!["invalid-json".into()], false, false).await;
    assert!(failed.is_err());
    assert_eq!(requests.len(), 3);
    let (recovered, requests) =
        run_split(dir.path(), 200, vec!["项目代号青松".into()], true, false).await;
    recovered.unwrap();
    assert_eq!(
        requests.len(),
        1,
        "invalid drafts must never become cached summaries"
    );
}

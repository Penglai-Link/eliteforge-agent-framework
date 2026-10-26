use async_trait::async_trait;
use eliteforge_agent_context::{CompressionOptions, ModelCompressor};
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::*;
use serde_json::json;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct ChineseSummary {
    drafts: Vec<String>,
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}
#[async_trait]
impl ChatModel for ChineseSummary {
    async fn generate(
        &self,
        _: &str,
        request: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        let index = {
            let mut requests = self.requests.lock().unwrap();
            let index = requests.len();
            requests.push(request);
            index
        };
        stream.text_delta("private summary draft").await;
        Ok(ChatResponse {
            content: Content(vec![ContentBlock::Json {
                value: json!({"summary": self.drafts[index.min(self.drafts.len() - 1)]}),
            }]),
            ..Default::default()
        })
    }
}
struct Limits {
    bytes: usize,
    calls: usize,
}
#[async_trait]
impl AgentLoop for Limits {
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let messages = vec![
            Message::user("项目代号青松；保留原始标识符。".repeat(300)),
            Message::assistant("已记录项目代号。"),
            Message::user("/compact"),
        ];
        let original = messages.clone();
        let history = ConversationSnapshot::new(
            ConversationKey::new(&request.session_id),
            "chinese",
            messages.clone(),
        );
        let model = ModelRef::new("test", "summary");
        let reducer = ModelCompressor::new(
            model.clone(),
            CompressionOptions {
                max_input_tokens: 64_000,
                max_summary_bytes: self.bytes,
                max_model_calls: self.calls,
            },
        )?;
        let projected = reducer
            .reduce(&ContextReductionInput {
                block: &ContextBlock {
                    id: HISTORY_BLOCK.into(),
                    messages,
                    trust: ContextTrust::Data,
                    source_version: None,
                },
                history: &history,
                run: context,
                models: services.models.as_deref().unwrap(),
                limits: &request.limits,
                budget: ContextBudget {
                    max_messages: 3,
                    max_tokens: 200,
                },
                model: &model,
                estimator: &UnicodeTokenEstimator,
                overflow_recoveries: 0,
                compression: Some(ContextCompressionDirective {
                    trigger: ContextCompressionTrigger::Global,
                    target_tokens: 200,
                }),
            })
            .await?;
        assert_eq!(
            history.messages, original,
            "source conversation must remain unchanged"
        );
        assert_eq!(
            projected.last(),
            original.last(),
            "current complete turn is protected"
        );
        let mut tokens = 0;
        for message in &projected {
            tokens += UnicodeTokenEstimator.message(&model, message).await?;
        }
        assert!(tokens <= 200);
        assert!(projected[0].content.text().contains("青松"));
        Ok(RunResult {
            output: "compressed".into(),
            content: Content::default(),
            structured: None,
            interactions: vec![],
            status: RunStatus::Completed,
            usage: context.usage(),
            steps: 0,
            tool_calls: 0,
            external_session: None,
        })
    }
}
async fn execute(
    drafts: Vec<String>,
    bytes: usize,
    calls: usize,
    success: bool,
) -> Vec<ChatRequest> {
    let provider = ChineseSummary {
        drafts,
        requests: Arc::default(),
    };
    let agent = TestBuilder::default()
        .plugin(ModelConnectionPlugin::new(ModelConnection::new(
            "test",
            Arc::new(provider.clone()),
            vec![ModelDescriptor::new("summary")],
        )))
        .plugin(AgentLoopPlugin::new(Limits { bytes, calls }))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("chinese", "unused");
    request.limits.max_model_output_tokens = 512;
    let result = agent.run(request).await;
    assert_eq!(result.is_ok(), success, "{result:?}");
    agent.framework().close().await;
    let requests = provider.requests.lock().unwrap().clone();
    for request in &requests {
        assert_eq!(
            request.options.max_output_tokens,
            Some(512),
            "reasoning allowance must remain independent"
        );
        assert!(request.tools.is_empty());
    }
    requests
}
fn parameters(request: &ChatRequest) -> &serde_json::Value {
    match &request.messages[1].content.0[0] {
        ContentBlock::Json { value } => value,
        _ => panic!("summary input must remain typed data"),
    }
}
#[tokio::test]
async fn chinese_bytes_can_exceed_token_allowance_without_being_over_budget() {
    let summary = "项目代号青松".repeat(10);
    let requests = execute(vec![summary.clone()], 4000, 16, true).await;
    assert_eq!(requests.len(), 1);
    let data = parameters(&requests[0]);
    assert_eq!(data["summary_max_utf8_bytes"], 4000);
    assert!(summary.len() > data["summary_max_tokens"].as_u64().unwrap() as usize);
    assert!(
        data["summary_target_characters"].as_u64().unwrap()
            < data["summary_max_tokens"].as_u64().unwrap()
    );
    assert!(data["overlong_draft"].is_null());
}
#[tokio::test]
async fn chinese_byte_overflow_recompresses_with_measured_feedback() {
    let requests = execute(vec!["测".repeat(30), "项目青松".into()], 64, 16, true).await;
    assert_eq!(requests.len(), 2);
    let retry = parameters(&requests[1]);
    assert_eq!(retry["overlong_draft"]["actual_utf8_bytes"], 90);
    assert!(
        retry["overlong_draft"]["actual_tokens"].as_u64().unwrap()
            <= retry["summary_max_tokens"].as_u64().unwrap()
    );
    assert!(
        retry["summary_target_characters"].as_u64().unwrap()
            < parameters(&requests[0])["summary_target_characters"]
                .as_u64()
                .unwrap()
    );
    assert_eq!(retry["history"], parameters(&requests[0])["history"]);
}
#[tokio::test]
async fn chinese_token_overflow_recompresses_even_when_utf8_bytes_fit() {
    let requests = execute(vec!["项".repeat(100), "青松".into()], 4000, 16, true).await;
    assert_eq!(requests.len(), 2);
    let retry = parameters(&requests[1]);
    assert!(
        retry["overlong_draft"]["actual_tokens"].as_u64().unwrap()
            > retry["summary_max_tokens"].as_u64().unwrap()
    );
    assert_eq!(retry["overlong_draft"]["actual_utf8_bytes"], 300);
}
#[tokio::test]
async fn overlong_chinese_retries_are_bounded_and_respect_total_call_budget() {
    assert_eq!(
        execute(vec!["测".repeat(1000)], 64, 16, false).await.len(),
        3
    );
    assert_eq!(
        execute(vec!["测".repeat(1000)], 64, 1, false).await.len(),
        1
    );
}

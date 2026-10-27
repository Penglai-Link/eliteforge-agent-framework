use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::TestBuilder;

#[derive(Clone, Copy)]
enum ExternalOutcome {
    NotStarted,
    Ambiguous,
    UnknownUsage,
}
#[async_trait]
impl ExternalAgent for ExternalOutcome {
    async fn run(
        &self,
        _: ExternalAgentRequest,
        _: &dyn ExternalAgentEventSink,
    ) -> Result<ExternalAgentResult, AgentError> {
        match self {
            Self::NotStarted => Err(AgentError::ExternalNotStarted(
                "configured working directory is required".into(),
            )),
            Self::Ambiguous => Err(AgentError::Run("connection lost after dispatch".into())),
            Self::UnknownUsage => Ok(ExternalAgentResult {
                content: "external done".into(),
                structured: None,
                session: None,
                usage: Usage::default(),
            }),
        }
    }
}
struct ReportedUsage(u64);
#[async_trait]
impl ChatModel for ReportedUsage {
    async fn generate(
        &self,
        _: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        Ok(ChatResponse {
            content: "parent recovered".into(),
            usage: Usage {
                input_tokens: self.0,
                output_tokens: 2,
            },
            ..Default::default()
        })
    }
}
struct RecoveringParent {
    external: Agent,
    native: Agent,
    native_calls: usize,
}
#[async_trait]
impl AgentLoop for RecoveringParent {
    fn requires_conversations(&self) -> bool {
        false
    }
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let _ = self
            .external
            .run_child(
                RunRequest::new(format!("{}-external", request.session_id), "work"),
                context,
            )
            .await;
        let mut output = String::new();
        for index in 0..self.native_calls {
            output = self
                .native
                .run_child(
                    RunRequest::new(format!("{}-native-{index}", request.session_id), "continue"),
                    context,
                )
                .await?
                .output;
        }
        Ok(RunResult {
            content: output.clone().into(),
            output,
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
async fn run(
    outcome: ExternalOutcome,
    ceiling: u64,
    reported: u64,
    calls: usize,
) -> (Result<RunResult, AgentError>, Usage) {
    let external = TestBuilder::new()
        .default_models(false)
        .plugin(AgentLoopPlugin::new(ExternalAgentLoop))
        .plugin(ExternalAgentPlugin::new(outcome))
        .build()
        .await
        .unwrap();
    let native = TestBuilder::default()
        .default_models(false)
        .plugin(ChatModelPlugin::new(ReportedUsage(reported)))
        .build()
        .await
        .unwrap();
    let parent = TestBuilder::new()
        .default_models(false)
        .plugin(AgentLoopPlugin::new(RecoveringParent {
            external: external.clone(),
            native: native.clone(),
            native_calls: calls,
        }))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("root", "work");
    request.limits.max_total_tokens = ceiling;
    let handle = parent.start(request).unwrap();
    let control = parent.control(&handle).unwrap();
    let result = parent.wait(handle).await;
    let charged = parent.budget_usage(&control).unwrap();
    parent.close().await;
    native.close().await;
    external.close().await;
    (result, charged)
}
#[tokio::test]
async fn certain_startup_failure_releases_child_allowance_and_parent_can_continue() {
    let (result, charged) = run(ExternalOutcome::NotStarted, 500, 4, 1).await;
    let result = result.unwrap();
    assert_eq!(result.output, "parent recovered");
    assert_eq!(
        charged,
        Usage {
            input_tokens: 4,
            output_tokens: 2
        }
    );
    assert_eq!(result.usage, charged);
}
#[tokio::test]
async fn dispatched_failure_keeps_explicit_reservation_when_usage_is_unknown() {
    let (result, charged) = run(ExternalOutcome::Ambiguous, 500, 4, 1).await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("parent budget cannot admit")
    );
    assert_eq!(charged.input_tokens + charged.output_tokens, 500);
}
#[tokio::test]
async fn default_unlimited_run_survives_unknown_external_usage_and_large_cumulative_usage() {
    let ceiling = RunLimits::default().max_total_tokens;
    assert_eq!(ceiling, RunLimits::UNLIMITED_TOKENS);
    let (result, charged) = run(ExternalOutcome::UnknownUsage, ceiling, 1_200_000, 2).await;
    let result = result.unwrap();
    assert_eq!(result.output, "parent recovered");
    assert_eq!(
        charged,
        Usage {
            input_tokens: 2_400_000,
            output_tokens: 4
        }
    );
    assert_eq!(result.usage, charged);
}
#[test]
fn unlimited_parent_does_not_reserve_its_entire_capacity_for_a_child() {
    use eliteforge_agent_core::budget::{UsageLedger, UsagePermit};
    use std::sync::{Arc, Mutex};
    let ledger = Arc::new(Mutex::new(UsageLedger::default()));
    let limit = RunLimits::UNLIMITED_TOKENS;
    let (mut first, first_limit) =
        UsagePermit::reserve_child(ledger.clone(), limit, limit).unwrap();
    let (mut second, second_limit) =
        UsagePermit::reserve_child(ledger.clone(), limit, limit).unwrap();
    assert_eq!((first_limit, second_limit), (limit, limit));
    assert_eq!(
        ledger.lock().unwrap().including_reservations(),
        Usage::default()
    );
    first
        .settle_child(
            &Usage {
                input_tokens: 11,
                output_tokens: 7,
            },
            limit,
        )
        .unwrap();
    second
        .settle_child(
            &Usage {
                input_tokens: 3,
                output_tokens: 2,
            },
            limit,
        )
        .unwrap();
    assert_eq!(
        ledger.lock().unwrap().including_reservations(),
        Usage {
            input_tokens: 14,
            output_tokens: 9
        }
    );
}

struct DirectoryExternal;
#[async_trait]
impl ExternalAgent for DirectoryExternal {
    async fn run(
        &self,
        request: ExternalAgentRequest,
        _: &dyn ExternalAgentEventSink,
    ) -> Result<ExternalAgentResult, AgentError> {
        let directory = request
            .extensions
            .get::<ExternalWorkingDirectory>()
            .expect("requested directory reaches external driver");
        Ok(ExternalAgentResult {
            content: directory.0.clone().into(),
            structured: None,
            session: None,
            usage: Usage {
                input_tokens: 1,
                output_tokens: 1,
            },
        })
    }
}
#[tokio::test]
async fn external_loop_transports_the_explicit_process_directory() {
    let agent = TestBuilder::new()
        .default_models(false)
        .plugin(AgentLoopPlugin::new(ExternalAgentLoop))
        .plugin(ExternalAgentPlugin::new(DirectoryExternal))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("selected-directory", "work");
    request.working_directory = Some("/host/selected/task".into());
    let result = agent.run(request).await.unwrap();
    assert_eq!(result.output, "/host/selected/task");
    agent.close().await;
}

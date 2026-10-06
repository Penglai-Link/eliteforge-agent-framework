use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::TestStoragePlugin;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct Reply(&'static str);
#[async_trait]
impl AgentLoop for Reply {
    async fn run(
        &self,
        _: &RunRequest,
        _: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        Ok(RunResult {
            output: self.0.into(),
            content: self.0.into(),
            structured: None,
            interactions: vec![],
            status: RunStatus::Completed,
            usage: Usage::default(),
            steps: 1,
            tool_calls: 0,
            external_session: None,
        })
    }
}
struct Lifecycle(Arc<AtomicUsize>);
#[async_trait]
impl Plugin for Lifecycle {
    fn id(&self) -> &str {
        "lifecycle"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let count = self.0.clone();
        context.on_shutdown(move || async move {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        Ok(())
    }
}
fn selection(id: &str) -> AgentSelection {
    AgentSelection {
        agent_loop: Some(id.into()),
        ..Default::default()
    }
}
#[tokio::test]
async fn selection_replacement_preserves_shared_plugins_and_previous_agent_on_failure() {
    let shutdown = Arc::new(AtomicUsize::new(0));
    let framework = Framework::builder()
        .plugin(TestStoragePlugin::default())
        .plugin(AgentLoopPlugin::named("first", Reply("first")))
        .plugin(AgentLoopPlugin::named("second", Reply("second")))
        .plugin(Lifecycle(shutdown.clone()))
        .build()
        .await
        .unwrap();
    let agent = framework.agent(selection("first")).build().await.unwrap();
    let clone = agent.clone();
    let handle = agent.start(RunRequest::new("one", "hello")).unwrap();
    assert!(matches!(
        agent
            .reconfigure(framework.agent(selection("second")))
            .await,
        Err(ReconfigureError::Busy(_))
    ));
    agent.wait(handle).await.unwrap();
    agent
        .reconfigure(framework.agent(selection("second")))
        .await
        .unwrap();
    assert_eq!(
        clone
            .run(RunRequest::new("two", "hello"))
            .await
            .unwrap()
            .output,
        "second"
    );
    assert!(matches!(
        agent
            .reconfigure(framework.agent(selection("missing")))
            .await,
        Err(ReconfigureError::Rejected(_))
    ));
    assert_eq!(
        clone
            .run(RunRequest::new("three", "hello"))
            .await
            .unwrap()
            .output,
        "second"
    );
    agent.close().await;
    assert_eq!(shutdown.load(Ordering::SeqCst), 0);
    framework.close().await;
    assert_eq!(shutdown.load(Ordering::SeqCst), 1);
}

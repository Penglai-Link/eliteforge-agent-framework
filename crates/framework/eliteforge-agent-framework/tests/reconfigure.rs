use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Reply(&'static str);
#[async_trait]
impl ChatModel for Reply {
    async fn generate(
        &self,
        _: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        Ok(ChatResponse::text(self.0))
    }
}
fn configured(text: &'static str) -> AgentBuilder {
    with_storage(AgentBuilder::new()).plugin_factory(ConfiguredPluginFactory::new(
        "model.connection.default",
        vec![],
        move || Ok(Arc::new(ChatModelPlugin::new(Reply(text)))),
    ))
}
#[tokio::test]
async fn clones_observe_idle_replacement_and_unconsumed_handle_blocks_switch() {
    let agent = configured("first").build().await.unwrap();
    let clone = agent.clone();
    let handle = agent.start(RunRequest::new("one", "hello")).unwrap();
    assert!(
        matches!(agent.reconfigure(configured("second")).await, Err(ReconfigureError::Busy(blockers)) if blockers.iter().any(|b| b.kind == "task"))
    );
    agent.wait(handle).await.unwrap();
    agent.reconfigure(configured("second")).await.unwrap();
    assert_eq!(
        clone
            .run(RunRequest::new("two", "hello"))
            .await
            .unwrap()
            .output,
        "second"
    );
    agent.close().await;
}
struct Broken;
struct CountLifecycle(Arc<[AtomicUsize; 3]>);
#[async_trait]
impl Plugin for CountLifecycle {
    fn id(&self) -> &str {
        "count.raw"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        self.0[0].fetch_add(1, Ordering::SeqCst);
        let counts = self.0.clone();
        context.on_shutdown(move || async move {
            counts[2].fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        Ok(())
    }
    async fn ready(&self, _: &mut PluginReadyContext<'_>) -> Result<(), PluginError> {
        self.0[1].fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
#[tokio::test]
async fn retained_raw_plugin_lifecycle_runs_once_across_success_and_failed_candidate() {
    let counts = Arc::new(std::array::from_fn(|_| AtomicUsize::new(0)));
    let agent = configured("one")
        .plugin(CountLifecycle(counts.clone()))
        .build()
        .await
        .unwrap();
    agent.reconfigure(configured("two")).await.unwrap();
    let broken =
        configured("three").plugin_factory(ConfiguredPluginFactory::new("broken", vec![], || {
            Ok(Arc::new(Broken))
        }));
    assert!(matches!(
        agent.reconfigure(broken).await,
        Err(ReconfigureError::Restored(_))
    ));
    assert_eq!(
        counts
            .iter()
            .map(|c| c.load(Ordering::SeqCst))
            .collect::<Vec<_>>(),
        vec![1, 1, 0]
    );
    agent.close().await;
    assert_eq!(counts[2].load(Ordering::SeqCst), 1);
}
#[async_trait]
impl Plugin for Broken {
    fn id(&self) -> &str {
        "broken"
    }
    async fn install(&self, _: &mut PluginContext<'_>) -> Result<(), PluginError> {
        Err(PluginError::message("candidate failed"))
    }
}
#[tokio::test]
async fn failed_candidate_restores_previous_configuration_with_fresh_factories() {
    let builds = Arc::new(AtomicUsize::new(0));
    let counter = builds.clone();
    let previous = with_storage(AgentBuilder::new()).plugin_factory(ConfiguredPluginFactory::new(
        "model.connection.default",
        vec![],
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(ChatModelPlugin::new(Reply("original"))))
        },
    ));
    let agent = previous.build().await.unwrap();
    let target = configured("candidate").plugin_factory(ConfiguredPluginFactory::new(
        "broken",
        vec![],
        || Ok(Arc::new(Broken)),
    ));
    assert!(matches!(
        agent.reconfigure(target).await,
        Err(ReconfigureError::Restored(_))
    ));
    assert_eq!(builds.load(Ordering::SeqCst), 2);
    assert_eq!(
        agent
            .run(RunRequest::new("restored", "hello"))
            .await
            .unwrap()
            .output,
        "original"
    );
    agent.close().await;
}
#[tokio::test]
async fn raw_plugins_are_never_silently_reinstalled() {
    let agent = with_storage(AgentBuilder::new())
        .plugin(ChatModelPlugin::new(Reply("raw")))
        .build()
        .await
        .unwrap();
    assert!(matches!(
        agent.reconfigure(configured("new")).await,
        Err(ReconfigureError::Invalid(_))
    ));
    assert_eq!(
        agent
            .run(RunRequest::new("still-running", "hello"))
            .await
            .unwrap()
            .output,
        "raw"
    );
    agent.close().await;
}

fn with_storage(builder: AgentBuilder) -> AgentBuilder {
    builder.plugin_factory(ConfiguredPluginFactory::new("test.sqlite", vec![], || {
        Ok(Arc::new(TestStoragePlugin::default()))
    }))
}

#[tokio::test]
async fn omitted_raw_members_keep_their_resources_and_are_not_reinstalled() {
    let agent = with_storage(AgentBuilder::new())
        .plugin(ChatModelPlugin::new(Reply("retained")))
        .build()
        .await
        .unwrap();
    agent
        .reconfigure(with_storage(AgentBuilder::new()))
        .await
        .unwrap();
    assert_eq!(
        agent
            .run(RunRequest::new("same-raw", "hello"))
            .await
            .unwrap()
            .output,
        "retained"
    );
    agent.close().await;
}

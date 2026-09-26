use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::TestConversationStore;
use std::sync::{Arc, Mutex};

type Log = Arc<Mutex<Vec<String>>>;

struct Recorded {
    id: &'static str,
    dependency: Option<&'static str>,
    fail: bool,
    log: Log,
}

#[async_trait]
impl Plugin for Recorded {
    fn id(&self) -> &str {
        self.id
    }
    fn dependencies(&self) -> Vec<String> {
        self.dependency.into_iter().map(str::to_owned).collect()
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("install {}", self.id));
        let id = self.id;
        let log = self.log.clone();
        context.on_shutdown(move || async move {
            log.lock().unwrap().push(format!("close {id}"));
            Ok(())
        });
        if self.fail {
            return Err(PluginError::message("pack member failed"));
        }
        Ok(())
    }
}

fn recorded(id: &'static str, dependency: Option<&'static str>, log: &Log) -> Recorded {
    Recorded {
        id,
        dependency,
        fail: false,
        log: log.clone(),
    }
}

struct UnusedLoop;
#[async_trait]
impl AgentLoop for UnusedLoop {
    async fn run(
        &self,
        _: &RunRequest,
        _: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        unreachable!("these tests only build and close agents")
    }
}

struct ApplicationPack(Log);
impl CapabilityPack for ApplicationPack {
    fn id(&self) -> &str {
        "test.application"
    }
    fn configuration(&self) -> &CapabilityConfig {
        &()
    }
    fn included_packs(&self) -> Vec<Arc<dyn CapabilityPack>> {
        vec![Arc::new(
            CapabilityPackBuilder::new("test.nested", ()).plugin(recorded("nested", None, &self.0)),
        )]
    }
    fn plugins(&self) -> Vec<Arc<dyn Plugin>> {
        test_pack()
            .plugin(recorded("dependent", Some("outside"), &self.0))
            .plugins()
    }
}

#[tokio::test]
async fn nested_packs_and_individual_plugins_share_dependency_order_and_shutdown() {
    let log = Log::default();
    let builder = AgentBuilder::new()
        .default_models(false)
        .capability_pack(ApplicationPack(log.clone()))
        .plugin_arc(Arc::new(recorded("outside", Some("nested"), &log)))
        .capability_pack(test_pack().plugin(AgentLoopPlugin::new(UnusedLoop)).plugin(
            ConversationStorePlugin::new(TestConversationStore::default()),
        ));
    assert!(
        log.lock().unwrap().is_empty(),
        "configuration must not install plugins"
    );
    let agent = builder.build().await.unwrap();
    assert_eq!(
        *log.lock().unwrap(),
        ["install nested", "install outside", "install dependent"]
    );
    assert!(agent.close().await.is_success());
    assert!(agent.close().await.is_success());
    assert_eq!(
        *log.lock().unwrap(),
        [
            "install nested",
            "install outside",
            "install dependent",
            "close dependent",
            "close outside",
            "close nested",
        ]
    );
}

#[tokio::test]
async fn duplicates_across_packs_and_individual_plugins_fail_before_installation() {
    for between_packs in [false, true] {
        let log = Log::default();
        let builder = AgentBuilder::new().capability_pack(test_pack().plugin(recorded(
            "duplicate",
            None,
            &log,
        )));
        let builder = if between_packs {
            builder.capability_pack(test_pack().plugin(recorded("duplicate", None, &log)))
        } else {
            builder.plugin(recorded("duplicate", None, &log))
        };
        let error = builder.build().await.err().expect("duplicate must fail");
        assert!(
            matches!(error, AgentError::Plugin(PluginError::DuplicatePluginId { id }) if id == "duplicate")
        );
        assert!(log.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn missing_dependencies_and_cycles_across_pack_boundaries_fail_before_installation() {
    let log = Log::default();
    let error = AgentBuilder::new()
        .capability_pack(ApplicationPack(log.clone()))
        .build()
        .await
        .err()
        .expect("missing dependency must fail");
    assert!(
        matches!(error, AgentError::Plugin(PluginError::MissingDependency { dependency, .. }) if dependency == "outside")
    );
    let error = AgentBuilder::new()
        .capability_pack(ApplicationPack(log.clone()))
        .plugin(recorded("outside", Some("dependent"), &log))
        .build()
        .await
        .err()
        .expect("cycle must fail");
    assert!(matches!(
        error,
        AgentError::Plugin(PluginError::DependencyCycle { .. })
    ));
    assert!(log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn failed_pack_member_rolls_back_individual_and_nested_members_in_reverse_order() {
    let log = Log::default();
    let error = AgentBuilder::new()
        .plugin(recorded("outside", None, &log))
        .capability_pack(
            test_pack()
                .capability_pack(test_pack().plugin(recorded("nested", Some("outside"), &log)))
                .plugin(Recorded {
                    fail: true,
                    ..recorded("failure", Some("nested"), &log)
                })
                .plugin(recorded("unreachable", Some("failure"), &log)),
        )
        .build()
        .await
        .err()
        .expect("installation must fail");
    assert!(
        matches!(error, AgentError::Plugin(PluginError::InstallationFailed { plugin, .. }) if plugin == "failure")
    );
    assert_eq!(
        *log.lock().unwrap(),
        [
            "install outside",
            "install nested",
            "install failure",
            "close failure",
            "close nested",
            "close outside",
        ]
    );
}

fn test_pack() -> CapabilityPackBuilder<()> {
    static ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    CapabilityPackBuilder::new(
        format!(
            "test.pack.{}",
            ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ),
        (),
    )
}

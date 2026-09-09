//! Acceptance coverage for cross-crate capability discovery and assembly.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use eliteforge_agent_capability_test_fixture as fixture;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::TestConversationStore;

fn builder(agent_loop: impl AgentLoop + 'static) -> AgentBuilder {
    AgentBuilder::new()
        .default_models(false)
        .plugin(AgentLoopPlugin::new(agent_loop))
        .plugin(ConversationStorePlugin::new(
            TestConversationStore::default(),
        ))
}

fn complete_result() -> RunResult {
    RunResult {
        output: String::new(),
        content: Content::default(),
        structured: None,
        status: RunStatus::Completed,
        interactions: Vec::new(),
        usage: Usage::default(),
        steps: 0,
        tool_calls: 0,
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

struct InspectContexts(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl AgentLoop for InspectContexts {
    async fn run(
        &self,
        _: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let mut contents = Vec::new();
        for provider in &services.context {
            contents.extend(
                provider
                    .context(context)
                    .await?
                    .into_iter()
                    .map(|message| message.content.text()),
            );
        }
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend(contents);
        Ok(complete_result())
    }
}

struct RecordedPlugin {
    id: &'static str,
    state: Arc<fixture::FixtureState>,
}

impl RecordedPlugin {
    fn new(id: &'static str, state: Arc<fixture::FixtureState>) -> Self {
        Self { id, state }
    }
}

#[async_trait]
impl Plugin for RecordedPlugin {
    fn id(&self) -> &str {
        self.id
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        self.state.record(format!("install:{}", self.id));
        let id = self.id;
        let state = self.state.clone();
        context.on_shutdown(move || async move {
            state.record(format!("close:{id}"));
            Ok(())
        });
        Ok(())
    }
}

fn capability_error(error: AgentError, expected: &str) {
    let message = error.to_string();
    assert!(
        matches!(error, AgentError::Capability(_)),
        "expected a capability error, got {message}"
    );
    assert!(
        message.contains(expected),
        "expected `{expected}` in capability error `{message}`"
    );
}

#[tokio::test]
async fn selected_pack_uses_its_agent_configuration_and_leaves_linked_payment_inert() {
    let state = Arc::new(fixture::FixtureState::default());
    let observed = Arc::new(Mutex::new(Vec::new()));
    let agent = builder(InspectContexts(observed.clone()))
        .capability_pack(fixture::shop_pack(fixture::FixtureConfig::new(
            "shop-a",
            state.clone(),
        )))
        .build()
        .await
        .unwrap();

    assert_eq!(state.factory_count("shop"), 1);
    assert_eq!(
        fixture::payment_factory_count(),
        0,
        "a linked but unselected package must not create its Default capability"
    );
    agent
        .run(RunRequest::new("shop-a", "inspect"))
        .await
        .unwrap();
    assert!(
        observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|message| message == "shop:shop-a"),
        "the config passed to this Agent must reach the configured factory"
    );
    assert!(agent.close().await.is_success());
}

#[tokio::test]
async fn explicitly_included_child_package_is_activated_with_its_own_configuration() {
    let state = Arc::new(fixture::FixtureState::default());
    let agent = builder(UnusedLoop)
        .capability_pack(fixture::shop_pack_with_audit(
            fixture::FixtureConfig::new("shop", state.clone()),
            fixture::FixtureConfig::new("audit", state.clone()),
        ))
        .build()
        .await
        .unwrap();

    assert_eq!(state.factory_count("shop"), 1);
    assert_eq!(state.factory_count("audit"), 1);
    assert!(
        state.events().contains(&"factory:audit:audit".to_owned()),
        "a child package must be selected explicitly by its parent, not merely linked"
    );
    assert!(agent.close().await.is_success());
}

#[tokio::test]
async fn dependencies_have_a_stable_order_despite_inventory_collection_order() {
    for run in 0..3 {
        let state = Arc::new(fixture::FixtureState::default());
        let label = format!("run-{run}");
        let agent = builder(UnusedLoop)
            .capability_pack(fixture::ordering_pack(fixture::FixtureConfig::new(
                label.clone(),
                state.clone(),
            )))
            .build()
            .await
            .unwrap();
        let order: Vec<_> = state
            .events()
            .into_iter()
            .filter(|event| event.starts_with("factory:ordered-"))
            .collect();
        assert_eq!(
            order,
            vec![
                format!("factory:ordered-first:{label}"),
                format!("factory:ordered-second:{label}"),
            ]
        );
        assert!(agent.close().await.is_success());
    }
}

#[tokio::test]
async fn one_implementation_contributing_two_interfaces_is_created_once() {
    let state = Arc::new(fixture::FixtureState::default());
    let observed = Arc::new(Mutex::new(Vec::new()));
    let agent = builder(InspectContexts(observed.clone()))
        .capability_pack(fixture::shared_pack(fixture::FixtureConfig::new(
            "one-instance",
            state.clone(),
        )))
        .build()
        .await
        .unwrap();

    assert_eq!(
        state.factory_count("shared"),
        1,
        "ContextProvider and EventSink adapters must share their declared instance"
    );
    agent
        .run(RunRequest::new("shared", "inspect"))
        .await
        .unwrap();
    assert!(
        observed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|message| message == "shared:one-instance")
    );
    assert!(agent.close().await.is_success());
}

#[tokio::test]
async fn two_agents_with_one_package_type_keep_configuration_and_lifecycle_isolated() {
    let state = Arc::new(fixture::FixtureState::default());
    let first_seen = Arc::new(Mutex::new(Vec::new()));
    let second_seen = Arc::new(Mutex::new(Vec::new()));
    let first = builder(InspectContexts(first_seen.clone()))
        .capability_pack(fixture::shop_pack(fixture::FixtureConfig::new(
            "first",
            state.clone(),
        )))
        .build()
        .await
        .unwrap();
    let second = builder(InspectContexts(second_seen.clone()))
        .capability_pack(fixture::shop_pack(fixture::FixtureConfig::new(
            "second",
            state.clone(),
        )))
        .build()
        .await
        .unwrap();

    assert_eq!(state.factory_count("shop"), 2);
    first
        .run(RunRequest::new("first", "inspect"))
        .await
        .unwrap();
    assert!(first.close().await.is_success());
    second
        .run(RunRequest::new("second", "inspect"))
        .await
        .unwrap();
    assert!(
        first_seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|message| message == "shop:first")
    );
    assert!(
        second_seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .any(|message| message == "shop:second"),
        "closing the first Agent must not close the second package instance"
    );
    assert!(second.close().await.is_success());
}

#[tokio::test]
async fn duplicate_ids_missing_dependencies_cycles_and_plugin_id_conflicts_fail_before_factories() {
    let duplicate_state = Arc::new(fixture::FixtureState::default());
    let error = builder(UnusedLoop)
        .capability_pack(fixture::duplicate_pack(fixture::FixtureConfig::new(
            "duplicate",
            duplicate_state.clone(),
        )))
        .build()
        .await
        .err()
        .expect("duplicate capability IDs must be rejected");
    capability_error(error, "duplicates.same-id");
    assert_eq!(duplicate_state.factory_count("duplicate-one"), 0);
    assert_eq!(duplicate_state.factory_count("duplicate-two"), 0);

    let missing_state = Arc::new(fixture::FixtureState::default());
    let error = builder(UnusedLoop)
        .capability_pack(fixture::missing_dependency_pack(
            fixture::FixtureConfig::new("missing", missing_state.clone()),
        ))
        .build()
        .await
        .err()
        .expect("missing dependencies must be rejected");
    capability_error(error, "not-declared");
    assert_eq!(missing_state.factory_count("missing-dependency"), 0);

    let cycle_state = Arc::new(fixture::FixtureState::default());
    let error = builder(UnusedLoop)
        .capability_pack(fixture::cycle_pack(fixture::FixtureConfig::new(
            "cycle",
            cycle_state.clone(),
        )))
        .build()
        .await
        .err()
        .expect("cycles must be rejected");
    capability_error(error, "cycle");
    assert_eq!(cycle_state.factory_count("cycle-a"), 0);
    assert_eq!(cycle_state.factory_count("cycle-b"), 0);

    let conflict_state = Arc::new(fixture::FixtureState::default());
    let error = builder(UnusedLoop)
        .plugin(RecordedPlugin::new(
            "plugin.conflict",
            conflict_state.clone(),
        ))
        .capability_pack(fixture::plugin_conflict_pack(fixture::FixtureConfig::new(
            "conflict",
            conflict_state.clone(),
        )))
        .build()
        .await
        .err()
        .expect("a capability may not shadow an ordinary plugin ID");
    capability_error(error, "plugin.conflict");
    assert_eq!(conflict_state.factory_count("plugin-conflict"), 0);
    assert!(
        conflict_state.events().is_empty(),
        "the ordinary plugin must not install after a preflight conflict"
    );
}

#[tokio::test]
async fn defaults_plugin_id_is_reserved_but_not_a_capability_dependency() {
    let conflict_state = Arc::new(fixture::FixtureState::default());
    let error = builder(UnusedLoop)
        .capability_pack(fixture::reserved_default_id_pack(
            fixture::FixtureConfig::new("reserved-id", conflict_state.clone()),
        ))
        .build()
        .await
        .err()
        .expect("a capability may not claim the defaults plugin ID");
    assert!(matches!(
        error,
        AgentError::Capability(CapabilityError::PluginIdConflict { capability })
            if capability == "builtin.framework-defaults"
    ));
    assert_eq!(conflict_state.factory_count("reserved-default-id"), 0);

    let dependency_state = Arc::new(fixture::FixtureState::default());
    let error = builder(UnusedLoop)
        .capability_pack(fixture::reserved_default_dependency_pack(
            fixture::FixtureConfig::new("reserved-dependency", dependency_state.clone()),
        ))
        .build()
        .await
        .err()
        .expect("a capability may not depend on the later defaults plugin");
    assert!(matches!(
        error,
        AgentError::Capability(CapabilityError::MissingDependency {
            capability,
            dependency,
        }) if capability == "reserved-default-dependency.context"
            && dependency == "builtin.framework-defaults"
    ));
    assert_eq!(
        dependency_state.factory_count("reserved-default-dependency"),
        0
    );
}

#[tokio::test]
async fn old_plugin_and_plugin_pack_participate_in_the_capability_dependency_graph() {
    let state = Arc::new(fixture::FixtureState::default());
    let agent = builder(UnusedLoop)
        .plugin_pack(
            PluginPackBuilder::new()
                .plugin(RecordedPlugin::new("legacy.dependency", state.clone())),
        )
        .plugin(RecordedPlugin::new("legacy.direct", state.clone()))
        .capability_pack(fixture::mixed_pack(fixture::FixtureConfig::new(
            "mixed",
            state.clone(),
        )))
        .build()
        .await
        .unwrap();

    let installed: Vec<_> = state
        .events()
        .into_iter()
        .filter(|event| event.starts_with("install:"))
        .collect();
    assert_eq!(
        installed,
        [
            "install:legacy.dependency",
            "install:legacy.direct",
            "install:mixed",
        ]
    );
    assert!(agent.close().await.is_success());
    let closed: Vec<_> = state
        .events()
        .into_iter()
        .filter(|event| event.starts_with("close:"))
        .collect();
    assert_eq!(
        closed,
        [
            "close:mixed",
            "close:legacy.direct",
            "close:legacy.dependency",
        ]
    );
}

#[tokio::test]
async fn capability_declared_plugin_failure_reuses_container_rollback_and_close() {
    let state = Arc::new(fixture::FixtureState::default());
    let error = builder(UnusedLoop)
        .plugin(RecordedPlugin::new("legacy.resource", state.clone()))
        .capability_pack(fixture::rollback_pack(fixture::FixtureConfig::new(
            "rollback",
            state.clone(),
        )))
        .build()
        .await
        .err()
        .expect("the capability plugin deliberately fails installation");

    assert!(matches!(
        error,
        AgentError::Plugin(PluginError::InstallationFailed { plugin, .. }) if plugin == "rollback.failure"
    ));
    assert_eq!(state.factory_count("rollback"), 1);
    let lifecycle: Vec<_> = state
        .events()
        .into_iter()
        .filter(|event| event.starts_with("install:") || event.starts_with("close:"))
        .collect();
    assert_eq!(
        lifecycle,
        [
            "install:legacy.resource",
            "install:rollback",
            "close:rollback",
            "close:legacy.resource",
        ]
    );
}

#[tokio::test]
async fn default_factory_is_shared_across_modules_and_different_impl_paths() {
    let agent = builder(UnusedLoop)
        .capability_pack(CapabilityPackBuilder::new("regression.default-shared", ()))
        .build()
        .await
        .unwrap();
    assert_eq!(
        fixture::regressions::DEFAULT_SHARED_CONSTRUCTIONS
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "one concrete type must have one default instance regardless of the impl module"
    );
    assert!(agent.close().await.is_success());
}

#[tokio::test]
async fn configured_factories_share_across_modules_and_aliases_with_implicit_or_explicit_keys() {
    for pack in ["regression.configured-shared", "regression.explicit-shared"] {
        let state = Arc::new(fixture::FixtureState::default());
        let observed = Arc::new(Mutex::new(Vec::new()));
        let agent = builder(InspectContexts(observed.clone()))
            .capability_pack(CapabilityPackBuilder::new(
                pack,
                fixture::FixtureConfig::new(pack, state.clone()),
            ))
            .build()
            .await
            .unwrap();
        assert_eq!(state.factory_count("cross-module"), 1);
        agent.run(RunRequest::new(pack, "inspect")).await.unwrap();
        assert_eq!(*observed.lock().unwrap(), [format!("cross-module:{pack}")]);
        assert!(
            state
                .events()
                .contains(&format!("event:cross-module:{pack}"))
        );
        assert!(agent.close().await.is_success());
    }
}

#[tokio::test]
async fn different_factory_functions_still_conflict_before_either_is_called() {
    let state = Arc::new(fixture::FixtureState::default());
    let error = builder(UnusedLoop)
        .capability_pack(CapabilityPackBuilder::new(
            "regression.factory-conflict",
            fixture::FixtureConfig::new("conflict", state.clone()),
        ))
        .build()
        .await
        .err()
        .expect("different factory functions cannot own one instance");
    assert!(matches!(
        error,
        AgentError::Capability(CapabilityError::InstanceFactoryConflict { .. })
    ));
    assert!(state.events().is_empty());
}

#[tokio::test]
async fn stacked_declarations_create_two_independent_instances_of_one_interface() {
    let state = Arc::new(fixture::FixtureState::default());
    let observed = Arc::new(Mutex::new(Vec::new()));
    let agent = builder(InspectContexts(observed.clone()))
        .capability_pack(CapabilityPackBuilder::new(
            "regression.repeated",
            fixture::FixtureConfig::new("repeated", state.clone()),
        ))
        .build()
        .await
        .unwrap();
    assert_eq!(state.factory_count("repeated"), 2);
    agent
        .run(RunRequest::new("repeated", "inspect"))
        .await
        .unwrap();
    assert_eq!(*observed.lock().unwrap(), ["repeated", "repeated"]);
    assert!(agent.close().await.is_success());
}

struct DeclaredDependencyPlugin {
    dependency: &'static str,
    state: Arc<fixture::FixtureState>,
}

#[async_trait]
impl Plugin for DeclaredDependencyPlugin {
    fn id(&self) -> &str {
        "regression.manual"
    }
    fn dependencies(&self) -> Vec<String> {
        vec![self.dependency.into()]
    }
    async fn install(&self, _: &mut PluginContext<'_>) -> Result<(), PluginError> {
        self.state.record("install:regression.manual");
        Ok(())
    }
}

#[tokio::test]
async fn mixed_plugin_cycle_and_missing_plugin_dependencies_fail_before_any_factory() {
    for dependency in ["regression.mixed.a", "regression.missing"] {
        let state = Arc::new(fixture::FixtureState::default());
        let error = builder(UnusedLoop)
            .plugin(DeclaredDependencyPlugin {
                dependency,
                state: state.clone(),
            })
            .capability_pack(CapabilityPackBuilder::new(
                "regression.mixed",
                fixture::FixtureConfig::new("mixed", state.clone()),
            ))
            .build()
            .await
            .err()
            .expect("invalid combined graph must fail preflight");
        match dependency {
            "regression.mixed.a" => assert!(matches!(
                error,
                AgentError::Plugin(PluginError::DependencyCycle { .. })
            )),
            _ => assert!(matches!(
                error,
                AgentError::Plugin(PluginError::MissingDependency { .. })
            )),
        }
        assert!(
            state.events().is_empty(),
            "neither capability factories nor ordinary plugins may run"
        );
    }
}

#[tokio::test]
async fn factory_order_follows_transitive_dependencies_through_ordinary_plugins() {
    let state = Arc::new(fixture::FixtureState::default());
    let agent = builder(UnusedLoop)
        .plugin(DeclaredDependencyPlugin {
            dependency: "regression.mixed.z",
            state: state.clone(),
        })
        .capability_pack(CapabilityPackBuilder::new(
            "regression.mixed",
            fixture::FixtureConfig::new("mixed", state.clone()),
        ))
        .build()
        .await
        .unwrap();
    assert_eq!(
        state
            .events()
            .into_iter()
            .filter(|event| event.starts_with("factory:"))
            .collect::<Vec<_>>(),
        [
            "factory:mixed-producer:mixed",
            "factory:mixed-consumer:mixed"
        ],
        "z -> ordinary plugin -> a must override alphabetical capability order"
    );
    assert!(agent.close().await.is_success());
}

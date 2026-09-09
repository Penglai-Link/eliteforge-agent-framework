//! Deliberately small business implementations linked from another crate.
//!
//! The framework integration tests use this crate to prove that capability
//! discovery crosses a crate boundary and that descriptors remain inert until
//! their package is selected for a particular Agent.

pub mod regressions;

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use eliteforge_agent_core::{
    AgentError, AgentEvent, CapabilityPackBuilder, ContextProvider, EventSink, Message, Plugin,
    PluginContext, PluginError, RunContext, capability,
};

/// Shared observability passed through test-only capability configuration.
#[derive(Default)]
pub struct FixtureState {
    factories: Mutex<BTreeMap<&'static str, usize>>,
    events: Mutex<Vec<String>>,
}

impl FixtureState {
    pub fn record_factory(&self, id: &'static str, configuration: &str) {
        *self
            .factories
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(id)
            .or_default() += 1;
        self.record(format!("factory:{id}:{configuration}"));
    }

    pub fn factory_count(&self, id: &str) -> usize {
        self.factories
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(id)
            .copied()
            .unwrap_or_default()
    }

    pub fn record(&self, event: impl Into<String>) {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(event.into());
    }

    pub fn events(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn clear_events(&self) {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
}

/// Every fixture package uses the same configuration shape so a test can prove
/// that each selected Agent receives its own value and state handle.
#[derive(Clone)]
pub struct FixtureConfig {
    pub label: String,
    pub state: Arc<FixtureState>,
}

impl FixtureConfig {
    pub fn new(label: impl Into<String>, state: Arc<FixtureState>) -> Self {
        Self {
            label: label.into(),
            state,
        }
    }
}

pub fn shop_pack(config: FixtureConfig) -> CapabilityPackBuilder<FixtureConfig> {
    CapabilityPackBuilder::new("shop", config)
}

pub fn shop_pack_with_audit(
    shop: FixtureConfig,
    audit: FixtureConfig,
) -> CapabilityPackBuilder<FixtureConfig> {
    CapabilityPackBuilder::new("shop", shop).include(CapabilityPackBuilder::new("audit", audit))
}

pub fn payment_pack(config: FixtureConfig) -> CapabilityPackBuilder<FixtureConfig> {
    CapabilityPackBuilder::new("payment", config)
}

pub fn ordering_pack(config: FixtureConfig) -> CapabilityPackBuilder<FixtureConfig> {
    CapabilityPackBuilder::new("ordering", config)
}

pub fn shared_pack(config: FixtureConfig) -> CapabilityPackBuilder<FixtureConfig> {
    CapabilityPackBuilder::new("shared", config)
}

pub fn duplicate_pack(config: FixtureConfig) -> CapabilityPackBuilder<FixtureConfig> {
    CapabilityPackBuilder::new("duplicates", config)
}

pub fn missing_dependency_pack(config: FixtureConfig) -> CapabilityPackBuilder<FixtureConfig> {
    CapabilityPackBuilder::new("missing-dependency", config)
}

pub fn cycle_pack(config: FixtureConfig) -> CapabilityPackBuilder<FixtureConfig> {
    CapabilityPackBuilder::new("cycle", config)
}

pub fn plugin_conflict_pack(config: FixtureConfig) -> CapabilityPackBuilder<FixtureConfig> {
    CapabilityPackBuilder::new("plugin-conflict", config)
}

pub fn mixed_pack(config: FixtureConfig) -> CapabilityPackBuilder<FixtureConfig> {
    CapabilityPackBuilder::new("mixed", config)
}

pub fn rollback_pack(config: FixtureConfig) -> CapabilityPackBuilder<FixtureConfig> {
    CapabilityPackBuilder::new("rollback", config)
}

pub fn reserved_default_id_pack(config: FixtureConfig) -> CapabilityPackBuilder<FixtureConfig> {
    CapabilityPackBuilder::new("reserved-default-id", config)
}

pub fn reserved_default_dependency_pack(
    config: FixtureConfig,
) -> CapabilityPackBuilder<FixtureConfig> {
    CapabilityPackBuilder::new("reserved-default-dependency", config)
}

pub struct ShopContext {
    label: String,
}

impl ShopContext {
    fn from_config(config: &FixtureConfig) -> Self {
        config.state.record_factory("shop", &config.label);
        Self {
            label: config.label.clone(),
        }
    }
}

#[capability(
    pack = "shop",
    id = "shop.context",
    instance = "shop.context",
    config = FixtureConfig,
    factory = ShopContext::from_config
)]
#[async_trait]
impl ContextProvider for ShopContext {
    async fn context(&self, _: &RunContext) -> Result<Vec<Message>, AgentError> {
        Ok(vec![Message::system(format!("shop:{}", self.label))])
    }
}

static PAYMENT_DEFAULT_CONSTRUCTIONS: AtomicUsize = AtomicUsize::new(0);

pub fn payment_factory_count() -> usize {
    PAYMENT_DEFAULT_CONSTRUCTIONS.load(Ordering::SeqCst)
}

pub struct PaymentContext;

impl Default for PaymentContext {
    fn default() -> Self {
        PAYMENT_DEFAULT_CONSTRUCTIONS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

#[capability(pack = "payment", id = "payment.context", instance = "payment.context")]
#[async_trait]
impl ContextProvider for PaymentContext {
    async fn context(&self, _: &RunContext) -> Result<Vec<Message>, AgentError> {
        Ok(vec![Message::system("payment")])
    }
}

pub struct AuditContext {
    label: String,
}

impl AuditContext {
    fn from_config(config: &FixtureConfig) -> Self {
        config.state.record_factory("audit", &config.label);
        Self {
            label: config.label.clone(),
        }
    }
}

#[capability(
    pack = "audit",
    id = "audit.context",
    instance = "audit.context",
    config = FixtureConfig,
    factory = AuditContext::from_config
)]
#[async_trait]
impl ContextProvider for AuditContext {
    async fn context(&self, _: &RunContext) -> Result<Vec<Message>, AgentError> {
        Ok(vec![Message::system(format!("audit:{}", self.label))])
    }
}

pub struct OrderedFirst;

impl OrderedFirst {
    fn from_config(config: &FixtureConfig) -> Self {
        config.state.record_factory("ordered-first", &config.label);
        Self
    }
}

#[capability(
    pack = "ordering",
    id = "ordering.first",
    instance = "ordering.first",
    config = FixtureConfig,
    factory = OrderedFirst::from_config
)]
#[async_trait]
impl ContextProvider for OrderedFirst {
    async fn context(&self, _: &RunContext) -> Result<Vec<Message>, AgentError> {
        Ok(vec![Message::system("ordering:first")])
    }
}

pub struct OrderedSecond;

impl OrderedSecond {
    fn from_config(config: &FixtureConfig) -> Self {
        config.state.record_factory("ordered-second", &config.label);
        Self
    }
}

#[capability(
    pack = "ordering",
    id = "ordering.second",
    instance = "ordering.second",
    depends_on = ["ordering.first"],
    config = FixtureConfig,
    factory = OrderedSecond::from_config
)]
#[async_trait]
impl ContextProvider for OrderedSecond {
    async fn context(&self, _: &RunContext) -> Result<Vec<Message>, AgentError> {
        Ok(vec![Message::system("ordering:second")])
    }
}

pub struct SharedCapability {
    label: String,
    state: Arc<FixtureState>,
}

impl SharedCapability {
    fn from_config(config: &FixtureConfig) -> Self {
        config.state.record_factory("shared", &config.label);
        Self {
            label: config.label.clone(),
            state: config.state.clone(),
        }
    }
}

#[capability(
    pack = "shared",
    id = "shared.context",
    instance = "shared.instance",
    config = FixtureConfig,
    factory = SharedCapability::from_config
)]
#[async_trait]
impl ContextProvider for SharedCapability {
    async fn context(&self, _: &RunContext) -> Result<Vec<Message>, AgentError> {
        Ok(vec![Message::system(format!("shared:{}", self.label))])
    }
}

#[capability(
    pack = "shared",
    id = "shared.events",
    instance = "shared.instance",
    config = FixtureConfig,
    factory = SharedCapability::from_config
)]
#[async_trait]
impl EventSink for SharedCapability {
    async fn on_event(&self, _: &RunContext, _: &AgentEvent) {
        self.state.record(format!("event:shared:{}", self.label));
    }
}

pub struct DuplicateOne;

impl DuplicateOne {
    fn from_config(config: &FixtureConfig) -> Self {
        config.state.record_factory("duplicate-one", &config.label);
        Self
    }
}

#[capability(
    pack = "duplicates",
    id = "duplicates.same-id",
    instance = "duplicates.one",
    config = FixtureConfig,
    factory = DuplicateOne::from_config
)]
#[async_trait]
impl ContextProvider for DuplicateOne {
    async fn context(&self, _: &RunContext) -> Result<Vec<Message>, AgentError> {
        Ok(Vec::new())
    }
}

pub struct DuplicateTwo;

impl DuplicateTwo {
    fn from_config(config: &FixtureConfig) -> Self {
        config.state.record_factory("duplicate-two", &config.label);
        Self
    }
}

#[capability(
    pack = "duplicates",
    id = "duplicates.same-id",
    instance = "duplicates.two",
    config = FixtureConfig,
    factory = DuplicateTwo::from_config
)]
#[async_trait]
impl ContextProvider for DuplicateTwo {
    async fn context(&self, _: &RunContext) -> Result<Vec<Message>, AgentError> {
        Ok(Vec::new())
    }
}

pub struct MissingDependency;

impl MissingDependency {
    fn from_config(config: &FixtureConfig) -> Self {
        config
            .state
            .record_factory("missing-dependency", &config.label);
        Self
    }
}

#[capability(
    pack = "missing-dependency",
    id = "missing-dependency.member",
    instance = "missing-dependency.member",
    depends_on = ["not-declared"],
    config = FixtureConfig,
    factory = MissingDependency::from_config
)]
#[async_trait]
impl ContextProvider for MissingDependency {
    async fn context(&self, _: &RunContext) -> Result<Vec<Message>, AgentError> {
        Ok(Vec::new())
    }
}

pub struct CycleA;

impl CycleA {
    fn from_config(config: &FixtureConfig) -> Self {
        config.state.record_factory("cycle-a", &config.label);
        Self
    }
}

#[capability(
    pack = "cycle",
    id = "cycle.a",
    instance = "cycle.a",
    depends_on = ["cycle.b"],
    config = FixtureConfig,
    factory = CycleA::from_config
)]
#[async_trait]
impl ContextProvider for CycleA {
    async fn context(&self, _: &RunContext) -> Result<Vec<Message>, AgentError> {
        Ok(Vec::new())
    }
}

pub struct CycleB;

impl CycleB {
    fn from_config(config: &FixtureConfig) -> Self {
        config.state.record_factory("cycle-b", &config.label);
        Self
    }
}

#[capability(
    pack = "cycle",
    id = "cycle.b",
    instance = "cycle.b",
    depends_on = ["cycle.a"],
    config = FixtureConfig,
    factory = CycleB::from_config
)]
#[async_trait]
impl ContextProvider for CycleB {
    async fn context(&self, _: &RunContext) -> Result<Vec<Message>, AgentError> {
        Ok(Vec::new())
    }
}

pub struct ConflictingPlugin {
    state: Arc<FixtureState>,
}

impl ConflictingPlugin {
    fn from_config(config: &FixtureConfig) -> Self {
        config
            .state
            .record_factory("plugin-conflict", &config.label);
        Self {
            state: config.state.clone(),
        }
    }
}

#[capability(
    pack = "plugin-conflict",
    id = "plugin.conflict",
    instance = "plugin.conflict",
    config = FixtureConfig,
    factory = ConflictingPlugin::from_config
)]
#[async_trait]
impl Plugin for ConflictingPlugin {
    fn id(&self) -> &str {
        "plugin.conflict"
    }

    async fn install(&self, _: &mut PluginContext<'_>) -> Result<(), PluginError> {
        self.state.record("install:plugin-conflict");
        Ok(())
    }
}

pub struct MixedPlugin {
    state: Arc<FixtureState>,
}

impl MixedPlugin {
    fn from_config(config: &FixtureConfig) -> Self {
        config.state.record_factory("mixed", &config.label);
        Self {
            state: config.state.clone(),
        }
    }
}

#[capability(
    pack = "mixed",
    id = "mixed.plugin",
    instance = "mixed.plugin",
    depends_on = ["legacy.dependency"],
    config = FixtureConfig,
    factory = MixedPlugin::from_config
)]
#[async_trait]
impl Plugin for MixedPlugin {
    fn id(&self) -> &str {
        "mixed.plugin"
    }

    fn dependencies(&self) -> Vec<String> {
        vec!["legacy.direct".into()]
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        self.state.record("install:mixed");
        let state = self.state.clone();
        context.on_shutdown(move || async move {
            state.record("close:mixed");
            Ok(())
        });
        Ok(())
    }
}

pub struct FailingPlugin {
    state: Arc<FixtureState>,
}

impl FailingPlugin {
    fn from_config(config: &FixtureConfig) -> Self {
        config.state.record_factory("rollback", &config.label);
        Self {
            state: config.state.clone(),
        }
    }
}

#[capability(
    pack = "rollback",
    id = "rollback.failure",
    instance = "rollback.failure",
    depends_on = ["legacy.resource"],
    config = FixtureConfig,
    factory = FailingPlugin::from_config
)]
#[async_trait]
impl Plugin for FailingPlugin {
    fn id(&self) -> &str {
        "rollback.failure"
    }

    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        self.state.record("install:rollback");
        let state = self.state.clone();
        context.on_shutdown(move || async move {
            state.record("close:rollback");
            Ok(())
        });
        Err(PluginError::message("fixture rollback failure"))
    }
}

pub struct ReservedDefaultId;

impl ReservedDefaultId {
    fn from_config(config: &FixtureConfig) -> Self {
        config
            .state
            .record_factory("reserved-default-id", &config.label);
        Self
    }
}

#[capability(
    pack = "reserved-default-id",
    id = "builtin.framework-defaults",
    instance = "reserved-default-id",
    config = FixtureConfig,
    factory = ReservedDefaultId::from_config
)]
#[async_trait]
impl ContextProvider for ReservedDefaultId {
    async fn context(&self, _: &RunContext) -> Result<Vec<Message>, AgentError> {
        Ok(Vec::new())
    }
}

pub struct ReservedDefaultDependency;

impl ReservedDefaultDependency {
    fn from_config(config: &FixtureConfig) -> Self {
        config
            .state
            .record_factory("reserved-default-dependency", &config.label);
        Self
    }
}

#[capability(
    pack = "reserved-default-dependency",
    id = "reserved-default-dependency.context",
    instance = "reserved-default-dependency",
    depends_on = ["builtin.framework-defaults"],
    config = FixtureConfig,
    factory = ReservedDefaultDependency::from_config
)]
#[async_trait]
impl ContextProvider for ReservedDefaultDependency {
    async fn context(&self, _: &RunContext) -> Result<Vec<Message>, AgentError> {
        Ok(Vec::new())
    }
}

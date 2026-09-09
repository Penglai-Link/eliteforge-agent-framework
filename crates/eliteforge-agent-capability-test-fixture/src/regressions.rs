//! Declarations that exercise macro hygiene and canonical type identity.

use super::*;

pub static DEFAULT_SHARED_CONSTRUCTIONS: AtomicUsize = AtomicUsize::new(0);

pub struct DefaultShared;

impl Default for DefaultShared {
    fn default() -> Self {
        DEFAULT_SHARED_CONSTRUCTIONS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

pub struct ConfiguredShared {
    label: String,
    state: Arc<FixtureState>,
}

impl ConfiguredShared {
    fn from_config(config: &FixtureConfig) -> Self {
        config.state.record_factory("cross-module", &config.label);
        Self {
            label: config.label.clone(),
            state: config.state.clone(),
        }
    }

    fn other_factory(config: &FixtureConfig) -> Self {
        config.state.record_factory("other-factory", &config.label);
        Self {
            label: config.label.clone(),
            state: config.state.clone(),
        }
    }
}

mod context_role {
    use super::*;

    #[capability(pack = "regression.default-shared")]
    #[async_trait]
    impl ContextProvider for DefaultShared {
        async fn context(&self, _: &RunContext) -> Result<Vec<Message>, AgentError> {
            Ok(vec![])
        }
    }

    #[capability(pack = "regression.configured-shared", config = FixtureConfig, factory = ConfiguredShared::from_config)]
    #[capability(pack = "regression.explicit-shared", instance = "regression.explicit", config = FixtureConfig, factory = ConfiguredShared::from_config)]
    #[capability(pack = "regression.factory-conflict", instance = "regression.conflict", config = FixtureConfig, factory = ConfiguredShared::from_config)]
    #[async_trait]
    impl ContextProvider for ConfiguredShared {
        async fn context(&self, _: &RunContext) -> Result<Vec<Message>, AgentError> {
            Ok(vec![Message::system(format!(
                "cross-module:{}",
                self.label
            ))])
        }
    }
}

mod event_role {
    use super::*;
    // Aliases and differing impl paths must still identify the same types.
    use super::ConfiguredShared as SharedAlias;
    type ConfigAlias = FixtureConfig;

    #[capability(pack = "regression.default-shared")]
    #[async_trait]
    impl EventSink for super::DefaultShared {
        async fn on_event(&self, _: &RunContext, _: &AgentEvent) {}
    }

    #[capability(pack = "regression.configured-shared", config = ConfigAlias, factory = SharedAlias::from_config)]
    #[capability(pack = "regression.explicit-shared", instance = "regression.explicit", config = ConfigAlias, factory = SharedAlias::from_config)]
    #[capability(pack = "regression.factory-conflict", instance = "regression.conflict", config = ConfigAlias, factory = SharedAlias::other_factory)]
    #[async_trait]
    impl EventSink for SharedAlias {
        async fn on_event(&self, _: &RunContext, _: &AgentEvent) {
            self.state
                .record(format!("event:cross-module:{}", self.label));
        }
    }
}

pub struct RepeatedProvider;

impl RepeatedProvider {
    fn from_config(config: &FixtureConfig) -> Self {
        config.state.record_factory("repeated", &config.label);
        Self
    }
}

#[capability(pack = "regression.repeated", id = "regression.repeated.first", instance = "regression.repeated.first", config = FixtureConfig, factory = RepeatedProvider::from_config)]
#[capability(pack = "regression.repeated", id = "regression.repeated.second", instance = "regression.repeated.second", config = FixtureConfig, factory = RepeatedProvider::from_config)]
#[async_trait]
impl ContextProvider for RepeatedProvider {
    async fn context(&self, _: &RunContext) -> Result<Vec<Message>, AgentError> {
        Ok(vec![Message::system("repeated")])
    }
}

pub struct MixedConsumer;

impl MixedConsumer {
    fn from_config(config: &FixtureConfig) -> Self {
        config.state.record_factory("mixed-consumer", &config.label);
        Self
    }
}

#[capability(pack = "regression.mixed", id = "regression.mixed.a", depends_on = ["regression.manual"], config = FixtureConfig, factory = MixedConsumer::from_config)]
#[async_trait]
impl ContextProvider for MixedConsumer {
    async fn context(&self, _: &RunContext) -> Result<Vec<Message>, AgentError> {
        Ok(vec![])
    }
}

pub struct MixedProducer;

impl MixedProducer {
    fn from_config(config: &FixtureConfig) -> Self {
        config.state.record_factory("mixed-producer", &config.label);
        Self
    }
}

#[capability(pack = "regression.mixed", id = "regression.mixed.z", config = FixtureConfig, factory = MixedProducer::from_config)]
#[async_trait]
impl ContextProvider for MixedProducer {
    async fn context(&self, _: &RunContext) -> Result<Vec<Message>, AgentError> {
        Ok(vec![])
    }
}

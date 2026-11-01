//! Host-owned candidate data and replaceable automatic routing policies.
use crate::*;
use async_trait::async_trait;
use std::{collections::BTreeMap, sync::Arc};

pub const CHAT_AUTO_ROUTE_NAME: &str = "对话自动路由";
pub const TEXT_GENERATION_AUTO_ROUTE_NAME: &str = "文本生成自动路由";
pub const TEXT_EMBEDDING_AUTO_ROUTE_NAME: &str = "文本向量自动路由";
pub const MULTIMODAL_EMBEDDING_AUTO_ROUTE_NAME: &str = "多模态向量自动路由";
pub const RERANK_AUTO_ROUTE_NAME: &str = "文本重排自动路由";
pub const MULTIMODAL_RERANK_AUTO_ROUTE_NAME: &str = "多模态重排自动路由";

#[derive(Debug, Clone)]
pub struct AutoRouteCandidate {
    pub model: ModelRef,
    pub input_modalities: Vec<Modality>,
    pub protocol: String,
    pub context_window_tokens: Option<usize>,
    /// Opaque policy data interpreted only by the installed router implementation.
    /// Framework dispatch never interprets supplier tags or quality rules.
    /// Contains no credentials or executable adapters.
    pub metadata: BTreeMap<String, serde_json::Value>,
}
impl AutoRouteCandidate {
    pub fn new(model: ModelRef) -> Self {
        Self {
            model,
            input_modalities: vec![Modality::Text],
            protocol: String::new(),
            context_window_tokens: None,
            metadata: BTreeMap::new(),
        }
    }
}

pub struct AutoRouteContext<'a> {
    pub operation: ModelOperation,
    pub stage: &'a str,
    /// Stable host scope (for example workspace/session/agent), never prompt text.
    pub affinity: &'a str,
    /// Invocation/stage/step identity, separate from stable session affinity.
    pub call_key: &'a str,
    pub previous: Option<&'a ModelRef>,
    pub transient_failures: usize,
}

/// Supply one bounded, authorized snapshot. Hosts may enrich or restrict the
/// available runtime candidates, but cannot grant access to unregistered models.
pub trait AutoRouteDataSource: Send + Sync {
    fn candidates(
        &self,
        context: &AutoRouteContext<'_>,
        available: &[AutoRouteCandidate],
    ) -> Result<Vec<AutoRouteCandidate>, AgentError>;
}

macro_rules! router_contract {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        pub trait $name: Send + Sync {
            fn candidates(&self, candidates: &[AutoRouteCandidate]) -> Vec<AutoRouteCandidate> {
                candidates.to_vec()
            }
            fn select(
                &self,
                context: &AutoRouteContext<'_>,
                candidates: &[AutoRouteCandidate],
            ) -> Result<ModelRef, AgentError>;
        }
    };
}
router_contract!(
    ChatAutoRouter,
    "对话自动路由。Model compatibility is enforced by the invoker."
);
router_contract!(
    TextGenerationAutoRouter,
    "文本生成自动路由。Independent of Agent conversation affinity and chat policies."
);
router_contract!(
    TextEmbeddingAutoRouter,
    "向量自动路由。Accepts plain text inputs only."
);
router_contract!(RerankAutoRouter, "Text reranking route.");
router_contract!(
    MultimodalRerankAutoRouter,
    "Multimodal reranking route, including text queries in the same candidate set."
);
router_contract!(
    MultimodalEmbeddingAutoRouter,
    "多模态向量自动路由。The host supplies candidates for the requested input modalities."
);

/// Opt-in policies, independently replaceable by the host or a capability pack.
/// The host snapshot is passed unchanged to the selected policy; dispatch checks
/// candidate membership without applying supplier or business filters.
#[derive(Clone, Default)]
pub struct AutoRouters {
    pub source: Option<Arc<dyn AutoRouteDataSource>>,
    pub chat: Option<Arc<dyn ChatAutoRouter>>,
    pub text_generation: Option<Arc<dyn TextGenerationAutoRouter>>,
    pub embedding: Option<Arc<dyn TextEmbeddingAutoRouter>>,
    pub multimodal_embedding: Option<Arc<dyn MultimodalEmbeddingAutoRouter>>,
    pub rerank: Option<Arc<dyn RerankAutoRouter>>,
    pub multimodal_rerank: Option<Arc<dyn MultimodalRerankAutoRouter>>,
}
enum RouteKind {
    Default,
    MultimodalEmbedding,
    MultimodalRerank,
    TextGeneration,
}
impl AutoRouters {
    pub fn select(
        &self,
        context: &AutoRouteContext<'_>,
        available: &[AutoRouteCandidate],
    ) -> Result<ModelRef, AgentError> {
        self.select_inner(context, available, RouteKind::Default)
    }
    pub fn select_text_generation(
        &self,
        context: &AutoRouteContext<'_>,
        available: &[AutoRouteCandidate],
    ) -> Result<ModelRef, AgentError> {
        if context.operation != ModelOperation::Chat {
            return Err(AgentError::InvalidConfiguration(
                "text generation route requires a chat model operation".into(),
            ));
        }
        self.select_inner(context, available, RouteKind::TextGeneration)
    }
    /// Select a multimodal embedding binding, including text queries in its shared space.
    pub fn select_multimodal_embedding(
        &self,
        context: &AutoRouteContext<'_>,
        available: &[AutoRouteCandidate],
    ) -> Result<ModelRef, AgentError> {
        if context.operation != ModelOperation::Embedding {
            return Err(AgentError::InvalidConfiguration(
                "multimodal embedding route requires an embedding operation".into(),
            ));
        }
        self.select_inner(context, available, RouteKind::MultimodalEmbedding)
    }
    pub fn select_multimodal_rerank(
        &self,
        context: &AutoRouteContext<'_>,
        available: &[AutoRouteCandidate],
    ) -> Result<ModelRef, AgentError> {
        if context.operation != ModelOperation::Rerank {
            return Err(AgentError::InvalidConfiguration(
                "multimodal rerank route requires a rerank operation".into(),
            ));
        }
        self.select_inner(context, available, RouteKind::MultimodalRerank)
    }
    fn select_inner(
        &self,
        context: &AutoRouteContext<'_>,
        available: &[AutoRouteCandidate],
        kind: RouteKind,
    ) -> Result<ModelRef, AgentError> {
        let candidates = match &self.source {
            Some(source) => source.candidates(context, available)?,
            None => available.to_vec(),
        };
        let available = available
            .iter()
            .map(|c| &c.model)
            .collect::<std::collections::BTreeSet<_>>();
        if candidates.iter().any(|c| !available.contains(&c.model)) {
            return Err(AgentError::InvalidConfiguration(
                "automatic route data source returned an unauthorized model".into(),
            ));
        }
        let selected = match context.operation {
            ModelOperation::Chat if matches!(kind, RouteKind::TextGeneration) => self
                .text_generation
                .as_ref()
                .ok_or(AgentError::MissingService("TextGenerationAutoRouter"))?
                .select(context, &candidates)?,
            ModelOperation::Chat => self
                .chat
                .as_ref()
                .ok_or(AgentError::MissingService("ChatAutoRouter"))?
                .select(context, &candidates)?,
            ModelOperation::Embedding if matches!(kind, RouteKind::MultimodalEmbedding) => self
                .multimodal_embedding
                .as_ref()
                .ok_or(AgentError::MissingService("MultimodalEmbeddingAutoRouter"))?
                .select(context, &candidates)?,
            ModelOperation::Embedding => self
                .embedding
                .as_ref()
                .ok_or(AgentError::MissingService("TextEmbeddingAutoRouter"))?
                .select(context, &candidates)?,
            ModelOperation::Rerank if matches!(kind, RouteKind::MultimodalRerank) => self
                .multimodal_rerank
                .as_ref()
                .ok_or(AgentError::MissingService("MultimodalRerankAutoRouter"))?
                .select(context, &candidates)?,
            ModelOperation::Rerank => self
                .rerank
                .as_ref()
                .ok_or(AgentError::MissingService("RerankAutoRouter"))?
                .select(context, &candidates)?,
            _ => {
                return Err(AgentError::InvalidConfiguration(
                    "operation has no automatic router".into(),
                ));
            }
        };
        if !candidates.iter().any(|c| c.model == selected) {
            return Err(AgentError::InvalidConfiguration(
                "automatic router selected a model outside its candidates".into(),
            ));
        }
        Ok(selected)
    }
}

pub struct AutoRoutersPlugin(pub AutoRouters);
#[async_trait]
impl Plugin for AutoRoutersPlugin {
    fn id(&self) -> &str {
        "model.auto-routers"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        context.provide(self.0.clone())
    }
}

/// Adapts 对话自动路由 to Agent model selection. Explicit defaults/stages keep
/// their normal precedence; RunRequest.model is handled before this strategy.
pub struct ChatAutoRoutingStrategy {
    pub routers: AutoRouters,
    pub routing: ModelRouting,
}
#[async_trait]
impl ModelSelectionStrategy for ChatAutoRoutingStrategy {
    fn validate(&self, catalog: &ModelCatalog) -> Result<(), AgentError> {
        CatalogModelStrategy(self.routing.clone()).validate(catalog)
    }
    async fn select(
        &self,
        context: &ModelSelectionContext<'_>,
        catalog: &ModelCatalog,
    ) -> Result<ModelRef, AgentError> {
        if let Some(model) = self.routing.selected(ModelOperation::Chat, context.stage) {
            return Ok(model.clone());
        }
        let candidates = catalog
            .models()
            .into_iter()
            .filter_map(|model| {
                let descriptor = catalog.get(&model)?;
                Some(AutoRouteCandidate {
                    input_modalities: descriptor.input_modalities.clone(),
                    protocol: catalog.protocol(&model).unwrap_or_default().into(),
                    model,
                    context_window_tokens: descriptor.context_window_tokens,
                    metadata: descriptor.metadata.clone(),
                })
            })
            .collect::<Vec<_>>();
        let call_key = format!("{:?}", (context.stage, context.step));
        self.routers.select(
            &AutoRouteContext {
                operation: ModelOperation::Chat,
                stage: context.stage,
                affinity: &context.run.session_id,
                call_key: &call_key,
                previous: context.previous,
                transient_failures: context.transient_failures,
            },
            &candidates,
        )
    }
}

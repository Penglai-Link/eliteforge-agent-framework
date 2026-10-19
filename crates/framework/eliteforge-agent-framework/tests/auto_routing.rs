use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::TestBuilder;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Last;
macro_rules! router {
    ($contract:ident) => {
        impl $contract for Last {
            fn select(
                &self,
                _: &AutoRouteContext<'_>,
                candidates: &[AutoRouteCandidate],
            ) -> Result<ModelRef, AgentError> {
                candidates
                    .last()
                    .map(|c| c.model.clone())
                    .ok_or_else(|| AgentError::Run("empty route".into()))
            }
        }
    };
}
router!(ChatAutoRouter);
router!(TextGenerationAutoRouter);
router!(TextEmbeddingAutoRouter);
router!(MultimodalEmbeddingAutoRouter);
router!(RerankAutoRouter);

struct Source(AtomicUsize);
impl AutoRouteDataSource for Source {
    fn candidates(
        &self,
        _: &AutoRouteContext<'_>,
        available: &[AutoRouteCandidate],
    ) -> Result<Vec<AutoRouteCandidate>, AgentError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(available
            .iter()
            .filter(|c| c.model.model == "b")
            .cloned()
            .collect())
    }
}
fn routers(source: Arc<Source>) -> AutoRouters {
    AutoRouters {
        source: Some(source),
        chat: Some(Arc::new(Last)),
        embedding: Some(Arc::new(Last)),
        rerank: Some(Arc::new(Last)),
        ..Default::default()
    }
}
struct Echo;
#[async_trait]
impl ChatModel for Echo {
    async fn generate(
        &self,
        model: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        Ok(ChatResponse::text(model))
    }
}
#[derive(Default)]
struct Embed {
    calls: AtomicUsize,
    fail_first: bool,
}
#[async_trait]
impl EmbeddingModel for Embed {
    async fn embed(
        &self,
        model: &str,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ModelError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_first && call == 0 {
            return Err(ModelError::Transient("retry".into()));
        }
        Ok(EmbeddingResponse {
            model: ModelRef::new("host", model),
            provider_model: None,
            vectors: request
                .inputs
                .iter()
                .enumerate()
                .map(|(index, _)| EmbeddingVector {
                    index,
                    values: vec![1.0; request.dimensions.unwrap_or(3)],
                })
                .collect(),
            input_tokens: None,
        })
    }
}
struct Rank;
#[async_trait]
impl RerankModel for Rank {
    async fn rerank(&self, model: &str, _: RerankRequest) -> Result<RerankResponse, ModelError> {
        Ok(RerankResponse {
            model: ModelRef::new("host", model),
            results: vec![RankedDocument {
                index: 0,
                score: 1.0,
            }],
        })
    }
}
fn update(embed: Arc<Embed>) -> ModelRuntimeUpdate {
    let mut update = ModelRuntimeUpdate::new("host", 1, vec![]);
    for model in ["a", "b"] {
        update.embedding.push(EmbeddingModelService::new(
            ModelRef::new("host", model),
            embed.clone(),
        ));
        update.rerank.push(RerankModelService::new(
            ModelRef::new("host", model),
            Arc::new(Rank),
        ));
    }
    update
}
fn rank_request() -> RerankRequest {
    RerankRequest {
        choice: Default::default(),
        query: "q".into(),
        documents: vec!["d".into()],
        top_n: Some(1),
    }
}

#[tokio::test]
async fn direct_text_generation_has_an_independent_router_and_authorized_source() {
    struct ChatMustNotRun;
    impl ChatAutoRouter for ChatMustNotRun {
        fn select(
            &self,
            _: &AutoRouteContext<'_>,
            _: &[AutoRouteCandidate],
        ) -> Result<ModelRef, AgentError> {
            panic!("direct text generation must not use the conversation router")
        }
    }
    let source = Arc::new(Source(AtomicUsize::new(0)));
    let client = ModelClient::new(ModelRuntimeUpdate::new(
        "host",
        1,
        vec![ModelConnection::new(
            "host",
            Arc::new(Echo),
            vec![ModelDescriptor::new("a"), ModelDescriptor::new("b")],
        )],
    ))
    .unwrap()
    .with_auto_routers(AutoRouters {
        source: Some(source.clone()),
        chat: Some(Arc::new(ChatMustNotRun)),
        text_generation: Some(Arc::new(Last)),
        ..Default::default()
    });
    let request = || ChatRequest {
        messages: vec![Message::user("Synthetic title request")],
        options: Default::default(),
        tools: vec![],
        extensions: Default::default(),
    };
    let response = client
        .text_generation(request(), &RunLimits::default(), "title")
        .await
        .unwrap();
    assert_eq!(response.content.text(), "b");
    assert_eq!(source.0.load(Ordering::SeqCst), 1);
    let missing = client.clone().with_auto_routers(AutoRouters {
        chat: Some(Arc::new(ChatMustNotRun)),
        ..Default::default()
    });
    assert!(matches!(
        missing
            .text_generation(request(), &RunLimits::default(), "title")
            .await,
        Err(AgentError::MissingService("TextGenerationAutoRouter"))
    ));
    struct Foreign;
    impl AutoRouteDataSource for Foreign {
        fn candidates(
            &self,
            _: &AutoRouteContext<'_>,
            _: &[AutoRouteCandidate],
        ) -> Result<Vec<AutoRouteCandidate>, AgentError> {
            Ok(vec![AutoRouteCandidate::new(ModelRef::new(
                "foreign",
                "unregistered",
            ))])
        }
    }
    let unauthorized = client.with_auto_routers(AutoRouters {
        source: Some(Arc::new(Foreign)),
        text_generation: Some(Arc::new(Last)),
        ..Default::default()
    });
    assert!(
        matches!(unauthorized.text_generation(request(), &RunLimits::default(), "title").await, Err(AgentError::InvalidConfiguration(message)) if message.contains("unauthorized"))
    );
}

#[tokio::test]
async fn text_generation_pins_authorization_before_the_router_runs() {
    struct BoundAnswer(&'static str);
    #[async_trait]
    impl ChatModel for BoundAnswer {
        async fn generate(
            &self,
            _: &str,
            _: ChatRequest,
            _: &dyn ChatStream,
        ) -> Result<ChatResponse, ModelError> {
            Ok(ChatResponse::text(self.0))
        }
    }
    fn update(answer: &'static str, revision: u64) -> ModelRuntimeUpdate {
        ModelRuntimeUpdate::new(
            answer,
            revision,
            vec![ModelConnection::new(
                "host",
                Arc::new(BoundAnswer(answer)),
                vec![ModelDescriptor::new("same-id")],
            )],
        )
    }
    struct SwapAccount(Arc<ModelRuntime>);
    impl TextGenerationAutoRouter for SwapAccount {
        fn select(
            &self,
            _: &AutoRouteContext<'_>,
            candidates: &[AutoRouteCandidate],
        ) -> Result<ModelRef, AgentError> {
            self.0.replace(update("new-account", 2))?;
            Ok(candidates[0].model.clone())
        }
    }
    let runtime = ModelRuntime::new(update("original-account", 1)).unwrap();
    let client = ModelClient::from_runtime(runtime.clone()).with_auto_routers(AutoRouters {
        text_generation: Some(Arc::new(SwapAccount(runtime.clone()))),
        ..Default::default()
    });
    let answer = client
        .text_generation(
            ChatRequest {
                messages: vec![Message::user("Synthetic title")],
                options: Default::default(),
                tools: vec![],
                extensions: Default::default(),
            },
            &RunLimits::default(),
            "title",
        )
        .await
        .unwrap();
    assert_eq!(answer.content.text(), "original-account");
    assert_eq!(runtime.snapshot().account_scope, "new-account");
}

#[tokio::test]
async fn agent_installs_all_three_routes_and_explicit_choices_bypass_them() {
    let source = Arc::new(Source(AtomicUsize::new(0)));
    let runtime = ModelRuntime::new(update(Arc::new(Embed::default()))).unwrap();
    let agent = TestBuilder::default()
        .plugin(ModelConnectionPlugin::new(ModelConnection::new(
            "host",
            Arc::new(Echo),
            vec![ModelDescriptor::new("a"), ModelDescriptor::new("b")],
        )))
        .plugin(ModelRuntimePlugin::new(runtime))
        .plugin(AutoRoutersPlugin(routers(source.clone())))
        .build()
        .await
        .unwrap();
    assert_eq!(
        agent
            .run(RunRequest::new("auto", "hello"))
            .await
            .unwrap()
            .output,
        "b"
    );
    assert_eq!(
        agent
            .embed(EmbeddingRequest::texts(["hello"]))
            .await
            .unwrap()
            .model
            .model,
        "b"
    );
    assert_eq!(agent.rerank(rank_request()).await.unwrap().model.model, "b");
    let calls = source.0.load(Ordering::SeqCst);
    let mut request = EmbeddingRequest::texts(["hello"]);
    request.choice.model = Some(ModelRef::new("host", "a"));
    assert_eq!(agent.embed(request).await.unwrap().model.model, "a");
    let mut request = rank_request();
    request.choice.model = Some(ModelRef::new("host", "a"));
    assert_eq!(agent.rerank(request).await.unwrap().model.model, "a");
    let mut request = RunRequest::new("explicit", "hello");
    request.model = Some(ModelRef::new("host", "a"));
    assert_eq!(agent.run(request).await.unwrap().output, "a");
    assert_eq!(source.0.load(Ordering::SeqCst), calls);
    agent.framework().close().await;
}

#[tokio::test]
async fn stage_defaults_and_index_space_remain_authoritative() {
    let source = Arc::new(Source(AtomicUsize::new(0)));
    let mut update = update(Arc::new(Embed::default()));
    let a = ModelRef::new("host", "a");
    update.routing = ModelRouting::default()
        .stage_for(ModelOperation::Embedding, "index", a.clone())
        .default_for(ModelOperation::Rerank, a.clone());
    let space = EmbeddingSpace {
        model: a.clone(),
        revision: "v1".into(),
        dimensions: 4,
        preprocessing: "plain/v1".into(),
    };
    update.spaces.push(space.clone());
    let client = ModelClient::new(update)
        .unwrap()
        .with_auto_routers(routers(source.clone()));
    let mut request = EmbeddingRequest::texts(["document"]);
    request.choice.stage = "index".into();
    assert_eq!(client.embed(request).await.unwrap().model, a);
    assert_eq!(client.rerank(rank_request()).await.unwrap().model, a);
    let query = client
        .embed_query(&space, EmbeddingRequest::texts(["query"]))
        .await
        .unwrap();
    assert_eq!(query.model, a);
    assert_eq!(query.vectors[0].values.len(), 4);
    assert_eq!(source.0.load(Ordering::SeqCst), 0);
    let mut invalid = space;
    invalid.revision = "v2".into();
    assert!(
        client
            .embed_query(&invalid, EmbeddingRequest::texts(["q"]))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn embedding_route_is_plain_text_only_and_retries_pin_the_selected_model() {
    let source = Arc::new(Source(AtomicUsize::new(0)));
    let adapter = Arc::new(Embed {
        calls: AtomicUsize::new(0),
        fail_first: true,
    });
    let client = ModelClient::new(update(adapter.clone()))
        .unwrap()
        .with_auto_routers(routers(source.clone()));
    let mut request = EmbeddingRequest::texts(["text"]);
    request.inputs = vec![Content(vec![ContentBlock::Json {
        value: serde_json::json!({"text":"not plain text"}),
    }])];
    assert!(client.embed(request).await.is_err());
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 0);
    let limits = RunLimits {
        max_model_retries: 1,
        ..Default::default()
    };
    let result = client
        .embed_observed(EmbeddingRequest::texts(["text"]), &limits)
        .await
        .into_result()
        .unwrap();
    assert_eq!(result.model.model, "b");
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 2);
    assert_eq!(source.0.load(Ordering::SeqCst), 1);
    assert_eq!(result.input_tokens, None);
}

struct Escape;

struct SwitchingEmbed {
    runtime: Arc<ModelRuntime>,
    calls: AtomicUsize,
    replacement: Arc<Embed>,
}
#[async_trait]
impl EmbeddingModel for SwitchingEmbed {
    async fn embed(
        &self,
        model: &str,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, ModelError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let mut next = ModelRuntimeUpdate::new("another-account", 2, vec![]);
            next.embedding.push(EmbeddingModelService::new(
                ModelRef::new("host", model),
                self.replacement.clone(),
            ));
            self.runtime.replace(next).unwrap();
            return Err(ModelError::Transient("catalog changed".into()));
        }
        Ok(EmbeddingResponse {
            model: ModelRef::new("host", model),
            provider_model: None,
            vectors: request
                .inputs
                .iter()
                .enumerate()
                .map(|(index, _)| EmbeddingVector {
                    index,
                    values: vec![1.0; 3],
                })
                .collect(),
            input_tokens: None,
        })
    }
}
#[tokio::test]
async fn retries_keep_the_original_authorization_binding_after_a_catalog_update() {
    let runtime =
        ModelRuntime::new(ModelRuntimeUpdate::new("original-account", 1, vec![])).unwrap();
    let replacement = Arc::new(Embed::default());
    let original = Arc::new(SwitchingEmbed {
        runtime: runtime.clone(),
        calls: AtomicUsize::new(0),
        replacement: replacement.clone(),
    });
    let mut initial = ModelRuntimeUpdate::new("original-account", 1, vec![]);
    initial.embedding.push(EmbeddingModelService::new(
        ModelRef::new("host", "b"),
        original.clone(),
    ));
    runtime.replace(initial).unwrap();
    let client = ModelClient::from_runtime(runtime.clone())
        .with_auto_routers(routers(Arc::new(Source(AtomicUsize::new(0)))));
    let limits = RunLimits {
        max_model_retries: 1,
        ..Default::default()
    };
    client
        .embed_observed(EmbeddingRequest::texts(["private input"]), &limits)
        .await
        .into_result()
        .unwrap();
    assert_eq!(runtime.snapshot().account_scope, "another-account");
    assert_eq!(original.calls.load(Ordering::SeqCst), 2);
    assert_eq!(replacement.calls.load(Ordering::SeqCst), 0);
}

impl AutoRouteDataSource for Escape {
    fn candidates(
        &self,
        _: &AutoRouteContext<'_>,
        _: &[AutoRouteCandidate],
    ) -> Result<Vec<AutoRouteCandidate>, AgentError> {
        Ok(vec![AutoRouteCandidate::new(ModelRef::new(
            "outside", "private",
        ))])
    }
}
impl ChatAutoRouter for Escape {
    fn select(
        &self,
        _: &AutoRouteContext<'_>,
        _: &[AutoRouteCandidate],
    ) -> Result<ModelRef, AgentError> {
        Ok(ModelRef::new("outside", "private"))
    }
}
#[test]
fn data_source_and_router_cannot_expand_authorization() {
    let context = AutoRouteContext {
        operation: ModelOperation::Chat,
        stage: "",
        affinity: "",
        call_key: "step",
        previous: None,
        transient_failures: 0,
    };
    let available = vec![AutoRouteCandidate::new(ModelRef::new("host", "a"))];
    let routers = AutoRouters {
        source: Some(Arc::new(Escape)),
        chat: Some(Arc::new(Last)),
        ..Default::default()
    };
    assert!(routers.select(&context, &available).is_err());
    let routers = AutoRouters {
        chat: Some(Arc::new(Escape)),
        ..Default::default()
    };
    assert!(routers.select(&context, &available).is_err());
}

#[tokio::test]
async fn multimodal_embedding_has_its_own_router_and_pins_query_space() {
    let source = Arc::new(Source(AtomicUsize::new(0)));
    let adapter = Arc::new(Embed::default());
    let mut registrations = update(adapter.clone());
    registrations.embedding[1].capabilities.input = vec![Modality::Text, Modality::Image];
    let space = EmbeddingSpace {
        model: ModelRef::new("host", "b"),
        revision: "image-v1".into(),
        dimensions: 3,
        preprocessing: "image/v1".into(),
    };
    registrations.spaces.push(space.clone());
    let client = ModelClient::new(registrations)
        .unwrap()
        .with_auto_routers(AutoRouters {
            source: Some(source.clone()),
            multimodal_embedding: Some(Arc::new(Last)),
            ..Default::default()
        });
    let mut image = EmbeddingRequest::texts(["caption"]);
    image.inputs[0].0.push(ContentBlock::Media {
        media: MediaContent {
            mime_type: "image/png".into(),
            source: MediaSource::Base64("AA==".into()),
            name: None,
            estimated_tokens: Some(10),
        },
    });
    let response = client.embed(image.clone()).await.unwrap();
    assert_eq!(response.model, space.model);
    assert_eq!(source.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        client
            .embed_query(&space, EmbeddingRequest::texts(["text query"]))
            .await
            .unwrap()
            .model,
        space.model
    );
    assert_eq!(source.0.load(Ordering::SeqCst), 1);
    // Installing only the text router cannot route an image, and no adapter is dispatched.
    let text_only = ModelClient::new(update(adapter.clone()))
        .unwrap()
        .with_auto_routers(routers(source.clone()));
    assert!(text_only.embed(image.clone()).await.is_err());
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 2);
    image.inputs[0].0.push(ContentBlock::Media {
        media: MediaContent {
            mime_type: "audio/wav".into(),
            source: MediaSource::Base64("AA==".into()),
            name: None,
            estimated_tokens: Some(10),
        },
    });
    assert!(client.embed(image).await.is_err());
    assert_eq!(adapter.calls.load(Ordering::SeqCst), 2);
}

struct ExactHostPool;
macro_rules! exact_host_pool {
    ($contract:ident) => {
        impl $contract for ExactHostPool {
            fn select(
                &self,
                _: &AutoRouteContext<'_>,
                candidates: &[AutoRouteCandidate],
            ) -> Result<ModelRef, AgentError> {
                assert_eq!(candidates.len(), 2);
                assert_eq!(candidates[0].model.connection, "supplier-one");
                assert_eq!(candidates[0].input_modalities, [Modality::Image]);
                assert_eq!(
                    candidates[0].metadata["supplier.custom-tag"],
                    serde_json::json!("opaque")
                );
                assert_eq!(candidates[1].model.connection, "supplier-two");
                Ok(candidates[1].model.clone())
            }
        }
    };
}
exact_host_pool!(ChatAutoRouter);
exact_host_pool!(TextEmbeddingAutoRouter);
exact_host_pool!(MultimodalEmbeddingAutoRouter);
exact_host_pool!(RerankAutoRouter);
#[test]
fn all_four_abstract_routes_pass_the_host_pool_through_without_supplier_rules() {
    let policy = Arc::new(ExactHostPool);
    let routers = AutoRouters {
        chat: Some(policy.clone()),
        embedding: Some(policy.clone()),
        multimodal_embedding: Some(policy.clone()),
        rerank: Some(policy),
        ..Default::default()
    };
    let mut first = AutoRouteCandidate::new(ModelRef::new("supplier-one", "first"));
    first.input_modalities = vec![Modality::Image];
    first
        .metadata
        .insert("supplier.custom-tag".into(), serde_json::json!("opaque"));
    let candidates = vec![
        first,
        AutoRouteCandidate::new(ModelRef::new("supplier-two", "second")),
    ];
    let mut context = AutoRouteContext {
        operation: ModelOperation::Chat,
        stage: "",
        affinity: "",
        call_key: "",
        previous: None,
        transient_failures: 0,
    };
    for operation in [
        ModelOperation::Chat,
        ModelOperation::Embedding,
        ModelOperation::Rerank,
    ] {
        context.operation = operation;
        assert_eq!(
            routers.select(&context, &candidates).unwrap(),
            candidates[1].model
        );
    }
    context.operation = ModelOperation::Embedding;
    assert_eq!(
        routers
            .select_multimodal_embedding(&context, &candidates)
            .unwrap(),
        candidates[1].model
    );
}

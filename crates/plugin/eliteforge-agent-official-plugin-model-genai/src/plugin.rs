use super::*;

#[derive(Clone, Copy)]
pub enum OpenAiChatProtocol {
    ChatCompletions,
    Responses,
}

/// Installs chat and embedding adapters. All credentials stay inside this plugin.
#[derive(Clone)]
pub struct GenaiPlugin {
    id: String,
    connection: String,
    adapter: AdapterKind,
    adapter_explicit: bool,
    endpoint: Option<String>,
    auth: Option<AuthData>,
    chat: Vec<ModelDescriptor>,
    embeddings: Vec<String>,
    configured: Option<Arc<GenaiChatAdapter>>,
    protocols: Vec<(String, OpenAiChatProtocol)>,
    environment: bool,
}
impl Default for GenaiPlugin {
    fn default() -> Self {
        Self {
            id: "model.genai.default".into(),
            connection: "default".into(),
            adapter: AdapterKind::OpenAI,
            adapter_explicit: false,
            endpoint: None,
            auth: None,
            chat: Vec::new(),
            embeddings: Vec::new(),
            configured: None,
            protocols: Vec::new(),
            environment: true,
        }
    }
}
impl GenaiPlugin {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            chat: vec![ModelDescriptor::new(model)],
            environment: false,
            ..Self::default()
        }
    }
    /// An explicit plugin with no chat model, for example for embeddings only.
    pub fn empty() -> Self {
        Self {
            environment: false,
            ..Self::default()
        }
    }
    pub fn connection(mut self, id: impl Into<String>) -> Self {
        self.connection = id.into();
        self.id = format!("model.genai.{}", self.connection);
        self
    }
    /// OpenAI and OpenAIResp select Chat Completions and Responses respectively.
    /// Embeddings independently use OpenAI's embeddings protocol in both cases.
    pub fn adapter(mut self, adapter: AdapterKind) -> Self {
        self.adapter = adapter;
        self.adapter_explicit = true;
        self
    }
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }
    pub fn auth(mut self, auth: AuthData) -> Self {
        self.auth = Some(auth);
        self
    }
    pub fn chat_model(mut self, model: ModelDescriptor) -> Self {
        self.chat.push(model);
        self
    }
    pub fn chat_model_with_protocol(
        mut self,
        model: ModelDescriptor,
        protocol: OpenAiChatProtocol,
    ) -> Self {
        self.protocols.push((model.id.clone(), protocol));
        self.chat.push(model);
        self
    }
    pub fn embedding_model(mut self, model: impl Into<String>) -> Self {
        self.embeddings.push(model.into());
        self
    }
    pub fn from_adapter(adapter: Arc<GenaiChatAdapter>, models: Vec<ModelDescriptor>) -> Self {
        Self {
            configured: Some(adapter.clone()),
            chat: models,
            adapter: adapter.adapter,
            ..Self::empty().connection(&adapter.connection_id)
        }
    }
    fn make_adapter(&self) -> Result<Arc<GenaiChatAdapter>, ModelError> {
        if let Some(adapter) = &self.configured {
            if adapter.connection_id != self.connection
                || adapter.adapter != self.adapter
                || self.endpoint.is_some()
                || self.auth.is_some()
            {
                return Err(ModelError::Fatal("configure transport on the supplied GenaiChatAdapter before constructing its plugin".into()));
            }
            return Ok(adapter.clone());
        }
        let kind = if self.environment && !self.adapter_explicit {
            match std::env::var("MODEL_ADAPTER").as_deref() {
                Ok("openai") | Err(std::env::VarError::NotPresent) => AdapterKind::OpenAI,
                Ok("openai-responses") => AdapterKind::OpenAIResp,
                Ok("anthropic") => AdapterKind::Anthropic,
                Ok("gemini") => AdapterKind::Gemini,
                Ok("ollama") => AdapterKind::Ollama,
                Ok("cohere") => AdapterKind::Cohere,
                _ => return Err(ModelError::Fatal("unsupported MODEL_ADAPTER".into())),
            }
        } else {
            self.adapter
        };
        let endpoint = self.endpoint.clone().or_else(|| {
            self.environment
                .then(|| std::env::var("MODEL_ENDPOINT").ok())
                .flatten()
        });
        let endpoint = match endpoint {
            Some(endpoint) => endpoint,
            None => match kind {
                AdapterKind::OpenAI | AdapterKind::OpenAIResp => "https://api.openai.com/v1/",
                AdapterKind::Anthropic => "https://api.anthropic.com/v1/",
                AdapterKind::Gemini => "https://generativelanguage.googleapis.com/v1beta/",
                AdapterKind::Ollama => "http://localhost:11434/",
                AdapterKind::Cohere => "https://api.cohere.com/v1/",
                _ => {
                    return Err(ModelError::Fatal(
                        "this adapter requires an explicit endpoint".into(),
                    ));
                }
            }
            .into(),
        };
        let auth = self.auth.clone().unwrap_or_else(|| {
            if self.environment && std::env::var_os("MODEL_API_KEY").is_some() {
                AuthData::from_env("MODEL_API_KEY")
            } else {
                kind.default_key_env_name()
                    .map_or(AuthData::None, AuthData::from_env)
            }
        });
        Ok(Arc::new(GenaiChatAdapter::new(
            &self.connection,
            kind,
            endpoint,
            auth,
        )?))
    }
}
#[async_trait]
impl Plugin for GenaiPlugin {
    fn id(&self) -> &str {
        &self.id
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let mut chat = self.chat.clone();
        let mut embeddings = self.embeddings.clone();
        if self.environment {
            if chat.is_empty()
                && let Ok(model) = std::env::var("MODEL")
            {
                chat.push(ModelDescriptor::new(model));
            }
            if embeddings.is_empty()
                && let Ok(model) = std::env::var("EMBEDDING_MODEL")
            {
                embeddings.push(model);
            }
        }
        if chat.is_empty() && embeddings.is_empty() {
            return Err(PluginError::message(
                "configure GenaiPlugin::new(model), MODEL or EMBEDDING_MODEL",
            ));
        }
        let adapter = self
            .make_adapter()
            .map_err(|e| PluginError::message(e.to_string()))?;
        if !chat.is_empty() {
            for model in &chat {
                if model.output_modalities.iter().any(|m| *m != Modality::Text) {
                    return Err(PluginError::message(
                        "this genai streaming adapter registers text output only",
                    ));
                }
                if model
                    .input_modalities
                    .iter()
                    .any(|m| !adapter.input_modalities().contains(m))
                {
                    return Err(PluginError::message(
                        "chat modality is unsupported by the selected genai protocol",
                    ));
                }
            }
            let mut connection = adapter.clone().connection(chat);
            for (model, protocol) in &self.protocols {
                if !matches!(
                    adapter.adapter,
                    AdapterKind::OpenAI | AdapterKind::OpenAIResp
                ) {
                    return Err(PluginError::message(
                        "OpenAI chat protocol overrides require an OpenAI connection",
                    ));
                }
                let kind = match protocol {
                    OpenAiChatProtocol::ChatCompletions => AdapterKind::OpenAI,
                    OpenAiChatProtocol::Responses => AdapterKind::OpenAIResp,
                };
                let override_adapter = Arc::new(
                    GenaiChatAdapter::new(
                        &self.connection,
                        kind,
                        adapter.endpoint.base_url(),
                        adapter.auth.clone(),
                    )
                    .map_err(|e| PluginError::message(e.to_string()))?
                    .with_client(adapter.client.clone())
                    .with_options(adapter.options.clone()),
                );
                connection = connection
                    .bind(model, override_adapter.clone(), override_adapter.protocol())
                    .map_err(|e| PluginError::message(e.to_string()))?;
            }
            let chat_model = connection.adapter.clone();
            context.contribute(ModelConnectionService(connection))?;
            context.contribute_extension(chat_model)?;
        }
        if !embeddings.is_empty() {
            let embed = Arc::new(
                GenaiEmbeddingAdapter::from_chat(&adapter)
                    .map_err(|e| PluginError::message(e.to_string()))?,
            );
            for model in embeddings {
                let registration = EmbeddingModelService::new(
                    ModelRef::new(&self.connection, model),
                    embed.clone(),
                )
                .protocol(embed.protocol());
                let embedding_model = registration.implementation.clone();
                context.contribute(registration)?;
                context.contribute_extension(embedding_model)?;
            }
        }
        Ok(())
    }
}

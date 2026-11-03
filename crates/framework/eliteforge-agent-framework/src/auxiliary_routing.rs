//! Shared modality filtering, logical identity selection and bounded failover.
use crate::*;
use std::collections::BTreeSet;

pub(crate) enum AuxiliaryRequest {
    Embedding(EmbeddingRequest),
    Rerank(RerankRequest),
}
pub(crate) enum AuxiliaryResponse {
    Embedding(EmbeddingResponse),
    Rerank(RerankResponse),
}

fn invalid(message: &str) -> AgentError {
    AgentError::InvalidConfiguration(message.into())
}

impl ModelClient {
    pub(crate) fn route_choice<T: ?Sized>(
        &self,
        operation: ModelOperation,
        catalog: &CapabilityCatalog<T>,
        choice: &ModelChoice,
        inputs: &[Content],
        compatible: impl Fn(&ModelRegistration<T>) -> bool,
        retry: (Option<&ModelRef>, usize),
    ) -> Result<ModelChoice, AgentError> {
        let mut required = choice.required_modalities.clone();
        for modality in inputs.iter().flat_map(Content::modalities) {
            if !required.contains(&modality) {
                required.push(modality);
            }
        }
        let eligible = |binding: &ModelRegistration<T>| {
            choice
                .model_code
                .as_ref()
                .is_none_or(|code| code == &binding.identity.model_code)
                && choice
                    .provider_id
                    .as_ref()
                    .is_none_or(|provider| provider == &binding.identity.provider_id)
                && required
                    .iter()
                    .all(|modality| binding.capabilities.input.contains(modality))
                && compatible(binding)
        };
        let fixed = choice.model.as_ref().or_else(|| {
            (choice.model_code.is_none() && choice.provider_id.is_none())
                .then(|| catalog.configured_model(choice))
                .flatten()
        });
        if let Some(model) = fixed {
            let binding = catalog.get(model).ok_or_else(|| {
                AgentError::Model(ModelError::Unavailable(
                    "configured model binding is unavailable".into(),
                ))
            })?;
            if !eligible(binding) {
                return Err(invalid(
                    "configured model conflicts with required modalities, provider, logical model or embedding space",
                ));
            }
            return Ok(ModelChoice {
                model: Some(model.clone()),
                ..choice.clone()
            });
        }
        let candidates = catalog
            .models()
            .filter_map(|model| {
                let binding = catalog.get(model)?;
                if !eligible(binding) {
                    return None;
                }
                let mut candidate = AutoRouteCandidate::new(model.clone());
                candidate.model_code = binding.identity.model_code.clone();
                candidate.provider_id = binding.identity.provider_id.clone();
                candidate.input_modalities = binding.capabilities.input.clone();
                candidate.protocol = binding.protocol.clone();
                Some(candidate)
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Err(invalid(
                "no authorized model satisfies the requested modalities, identity and embedding space",
            ));
        }
        let enabled = match operation {
            ModelOperation::Embedding => {
                self.auto_routers.embedding.is_some()
                    || self.auto_routers.multimodal_embedding.is_some()
            }
            ModelOperation::Rerank => {
                self.auto_routers.rerank.is_some() || self.auto_routers.multimodal_rerank.is_some()
            }
            _ => false,
        };
        let model = if enabled {
            self.auto_routers.select(
                &AutoRouteContext {
                    operation,
                    stage: &choice.stage,
                    affinity: self
                        .options
                        .run
                        .as_ref()
                        .map_or("", |run| run.session_id.as_str()),
                    call_key: &choice.stage,
                    previous: retry.0,
                    transient_failures: retry.1,
                },
                &candidates,
            )?
        } else if candidates.len() == 1 {
            candidates[0].model.clone()
        } else {
            return Err(invalid(
                "multiple compatible models require an installed automatic router or explicit model choice",
            ));
        };
        Ok(ModelChoice {
            model: Some(model),
            ..choice.clone()
        })
    }

    pub(crate) async fn execute_auxiliary(
        &self,
        mut request: AuxiliaryRequest,
    ) -> Result<AuxiliaryResponse, AgentError> {
        let snapshot = self.runtime.snapshot();
        let (operation, mut constraints, inputs) = match &request {
            AuxiliaryRequest::Embedding(request) => (
                ModelOperation::Embedding,
                request.choice.clone(),
                request.inputs.clone(),
            ),
            AuxiliaryRequest::Rerank(request) => {
                let mut inputs = request.documents.clone();
                inputs.push(request.query.clone());
                (ModelOperation::Rerank, request.choice.clone(), inputs)
            }
        };
        let mut client = self.with_runtime(ModelRuntime::frozen(snapshot.clone()));
        client.admitted = true;
        let retries = self
            .options
            .run
            .as_ref()
            .map_or(0, |run| run.limits().max_model_retries);
        let max = self.options.run.as_ref().map_or(1, |run| {
            run.limits()
                .max_model_calls_per_step
                .unwrap_or(retries.saturating_add(2))
        });
        let mut retry_binding = None;
        let mut refreshed = BTreeSet::new();
        let mut previous = None;
        let mut failures = 0;
        let mut last_error = None;
        for attempt in 0..max {
            if let Some(run) = &self.options.run {
                run.check_cancelled()?;
            }
            let mut selection = constraints.clone();
            if let Some(binding) = retry_binding.take() {
                selection.model = Some(binding);
            }
            let selected = match &request {
                AuxiliaryRequest::Embedding(request) => self.route_choice(
                    operation,
                    &snapshot.embedding,
                    &selection,
                    &inputs,
                    |binding| {
                        let registered = snapshot.spaces.get(&binding.model);
                        if registered.is_some_and(|space| {
                            request
                                .dimensions
                                .is_some_and(|dimension| dimension != space.dimensions)
                        }) {
                            return false;
                        }
                        request.space.as_ref().is_none_or(|expected| {
                            registered.is_some_and(|space| {
                                space.revision == expected.revision
                                    && space.dimensions == expected.dimensions
                                    && space.preprocessing == expected.preprocessing
                                    && match &expected.logical_model_code {
                                        Some(code) => {
                                            space.logical_model_code.as_ref() == Some(code)
                                                && &binding.identity.model_code == code
                                        }
                                        None => space == expected,
                                    }
                            })
                        })
                    },
                    (previous.as_ref(), failures),
                ),
                AuxiliaryRequest::Rerank(_) => self.route_choice(
                    operation,
                    &snapshot.rerank,
                    &selection,
                    &inputs,
                    |_| true,
                    (previous.as_ref(), failures),
                ),
            };
            let choice = match selected {
                Ok(choice) => choice,
                Err(error) => return Err(last_error.unwrap_or(error)),
            };
            let model = choice
                .model
                .as_ref()
                .expect("route resolved a concrete binding")
                .clone();
            let logical = match operation {
                ModelOperation::Embedding => {
                    &snapshot.embedding.get(&model).unwrap().identity.model_code
                }
                _ => &snapshot.rerank.get(&model).unwrap().identity.model_code,
            };
            // A retry may change provider, never the selected logical model/version.
            if constraints.model_code.is_none() {
                constraints.model_code = Some(logical.clone());
            }
            let result = match &mut request {
                AuxiliaryRequest::Embedding(request) => {
                    request.choice = choice;
                    client
                        .embed_once(request.clone())
                        .await
                        .map(AuxiliaryResponse::Embedding)
                }
                AuxiliaryRequest::Rerank(request) => {
                    request.choice = choice;
                    client
                        .rerank_once(request.clone())
                        .await
                        .map(AuxiliaryResponse::Rerank)
                }
            };
            let mut error = match result {
                Ok(response) => return Ok(response),
                Err(error) => error,
            };
            let model_error = match error.primary() {
                AgentError::Model(error) => Some(error.clone()),
                _ => None,
            };
            let authentication = matches!(&model_error, Some(ModelError::Provider(detail)) if detail.category == ProviderErrorCategory::Authentication);
            let mut retry = false;
            if attempt + 1 < max {
                if authentication && refreshed.insert(model.clone()) {
                    let retry_tokens = match &request {
                        AuxiliaryRequest::Embedding(request) => snapshot
                            .embedding
                            .get(&model)
                            .unwrap()
                            .implementation
                            .input_token_estimate(request),
                        AuxiliaryRequest::Rerank(request) => {
                            let adapter = &snapshot.rerank.get(&model).unwrap().implementation;
                            adapter
                                .input_token_estimate(request)
                                .saturating_add(adapter.output_token_reserve() as u64)
                        }
                    };
                    let admitted = self.options.run.as_ref().is_none_or(|run| {
                        run.usage
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .can_reserve(retry_tokens, run.token_ceiling)
                            && run
                                .extensions
                                .get::<ContinuationBudget>()
                                .is_none_or(|budget| budget.remaining_model_calls() > 0)
                    });
                    if !admitted {
                        match operation {
                            ModelOperation::Embedding => snapshot
                                .embedding
                                .get(&model)
                                .unwrap()
                                .implementation
                                .on_failure(model_error.as_ref().unwrap()),
                            _ => snapshot
                                .rerank
                                .get(&model)
                                .unwrap()
                                .implementation
                                .on_failure(model_error.as_ref().unwrap()),
                        }
                        return Err(error);
                    }
                    let recovery = async {
                        match operation {
                            ModelOperation::Embedding => {
                                snapshot
                                    .embedding
                                    .get(&model)
                                    .unwrap()
                                    .implementation
                                    .recover_authentication(model_error.as_ref().unwrap())
                                    .await
                            }
                            _ => {
                                snapshot
                                    .rerank
                                    .get(&model)
                                    .unwrap()
                                    .implementation
                                    .recover_authentication(model_error.as_ref().unwrap())
                                    .await
                            }
                        }
                    };
                    let recovered = tokio::select! {
                        biased;
                        _ = self.options.cancellation.cancelled() => return Err(AgentError::Cancelled),
                        result = tokio::time::timeout(self.options.timeout, recovery) => result.unwrap_or_else(|_| Err(ModelError::Fatal("credential refresh timed out".into()))),
                    };
                    match recovered {
                        Ok(true) => retry = true,
                        Ok(false) => {}
                        Err(failure) => error = failure.into(),
                    }
                    if retry {
                        retry_binding = Some(model.clone());
                    }
                } else if model_error.as_ref().is_some_and(ModelError::is_transient)
                    && attempt < retries
                {
                    retry = true;
                    failures += 1;
                }
            }
            if !retry {
                if let AgentError::Model(error) = error.primary() {
                    match operation {
                        ModelOperation::Embedding => snapshot
                            .embedding
                            .get(&model)
                            .unwrap()
                            .implementation
                            .on_failure(error),
                        _ => snapshot
                            .rerank
                            .get(&model)
                            .unwrap()
                            .implementation
                            .on_failure(error),
                    }
                }
                return Err(error);
            }
            previous = Some(model);
            last_error = Some(error);
            if let Some(run) = &self.options.run {
                run.emit(AgentEvent::ModelRetry {
                    step: 0,
                    attempt: attempt + 1,
                })
                .await;
            }
        }
        Err(last_error.unwrap_or_else(|| invalid("model call budget exhausted")))
    }
}

use eliteforge_agent_core::{AutoRouteCandidate, Modality, ModelRef};
use std::collections::BTreeMap;

/// One host-authorized PersonAI instance, with platform-declared metadata.
pub struct PersonAiEmbeddingModel {
    pub model: ModelRef,
    pub dimension: Option<usize>,
    pub tags: Vec<String>,
}
#[derive(Clone, Copy)]
pub enum PersonAiEmbeddingKind {
    Text,
    Multimodal,
}

/// Conservative provider request profile. Multimodal deployments admit at most
/// four items per request; callers may impose a smaller local budget.
pub fn personai_embedding_batch_limit(kind: PersonAiEmbeddingKind) -> usize {
    match kind {
        PersonAiEmbeddingKind::Text => 32,
        PersonAiEmbeddingKind::Multimodal => 4,
    }
}

/// PersonAI-specific tag semantics belong here, never in Framework Core.
/// A logical model is eligible only when every available instance agrees with
/// the requested capability and index dimension; instance rotation then stays in one space.
pub fn personai_embedding_candidates(
    models: impl IntoIterator<Item = PersonAiEmbeddingModel>,
    kind: PersonAiEmbeddingKind,
    dimension: Option<usize>,
) -> Vec<AutoRouteCandidate> {
    let mut groups = BTreeMap::<ModelRef, Vec<PersonAiEmbeddingModel>>::new();
    for model in models {
        groups.entry(model.model.clone()).or_default().push(model);
    }
    groups
        .into_iter()
        .filter_map(|(model, instances)| {
            let compatible = instances.iter().all(|instance| {
                dimension.is_none_or(|required| instance.dimension == Some(required))
                    && match kind {
                        PersonAiEmbeddingKind::Multimodal => instance
                            .tags
                            .iter()
                            .any(|tag| tag == "multimodal_embedding"),
                        PersonAiEmbeddingKind::Text => {
                            instance.tags.iter().any(|tag| tag == "text_embedding")
                                && !instance
                                    .tags
                                    .iter()
                                    .any(|tag| tag == "multimodal_embedding")
                        }
                    }
            });
            if !compatible {
                return None;
            }
            let mut candidate = AutoRouteCandidate::new(model);
            candidate.protocol = "personai.embeddings".into();
            candidate.input_modalities = match kind {
                PersonAiEmbeddingKind::Text => vec![Modality::Text],
                PersonAiEmbeddingKind::Multimodal => vec![Modality::Text, Modality::Image],
            };
            candidate
                .metadata
                .insert(super::PERSONAI_PROVIDER.into(), true.into());
            Some(candidate)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn instance(code: &str, tags: &[&str], dimension: Option<usize>) -> PersonAiEmbeddingModel {
        PersonAiEmbeddingModel {
            model: ModelRef::new("provider", code),
            tags: tags.iter().map(|tag| (*tag).into()).collect(),
            dimension,
        }
    }
    #[test]
    fn platform_tags_and_dimensions_define_the_pool_without_model_name_rules() {
        let models = || {
            vec![
                instance("arbitrary-name", &["multimodal_embedding"], Some(1024)),
                instance("Qwen3-VL-Embedding-2B", &["text_embedding"], Some(1024)),
                instance("unknown", &[], Some(1024)),
                instance("small-space", &["multimodal_embedding"], Some(512)),
            ]
        };
        assert_eq!(
            personai_embedding_candidates(models(), PersonAiEmbeddingKind::Multimodal, Some(1024))
                [0]
            .model
            .model,
            "arbitrary-name"
        );
        assert_eq!(
            personai_embedding_candidates(models(), PersonAiEmbeddingKind::Text, Some(1024))[0]
                .model
                .model,
            "Qwen3-VL-Embedding-2B"
        );
        assert!(
            personai_embedding_candidates(
                vec![
                    instance("same", &["multimodal_embedding"], Some(1024)),
                    instance("same", &["text_embedding"], Some(1024))
                ],
                PersonAiEmbeddingKind::Multimodal,
                Some(1024)
            )
            .is_empty()
        );
    }
}

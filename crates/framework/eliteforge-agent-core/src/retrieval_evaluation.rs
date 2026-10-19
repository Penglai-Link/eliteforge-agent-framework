//! Retrieval evaluation uses stable original evidence, never generated chunk IDs.
use crate::ChunkSourceSpan;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrievalEvidenceTarget {
    pub document_id: String,
    pub span: ChunkSourceSpan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EvaluationPartition {
    Training,
    Holdout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EvaluationProvenance {
    HumanLabeled,
    Generated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrievalEvaluationCase {
    pub id: String,
    pub query: String,
    pub expected: Vec<RetrievalEvidenceTarget>,
    pub partition: EvaluationPartition,
    pub provenance: EvaluationProvenance,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrievedEvidence {
    pub document_id: String,
    pub spans: Vec<ChunkSourceSpan>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrievalCaseScore {
    pub case_id: String,
    pub recall: f64,
    pub precision: f64,
    pub reciprocal_rank: f64,
    pub discounted_recall: f64,
    pub matched_targets: usize,
    pub total_targets: usize,
    pub missing: Vec<RetrievalEvidenceTarget>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetrievalEvaluationMetrics {
    pub cases: usize,
    pub recall: f64,
    pub precision: f64,
    pub mrr: f64,
    pub discounted_recall: f64,
    pub hit_rate: f64,
    pub human_labeled_cases: usize,
    pub generated_cases: usize,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct RetrievalEvaluationError(pub String);

/// Retrieval must run against the supplied candidate index. A transport/model
/// failure is an error, not a zero-recall observation.
#[async_trait::async_trait]
pub trait RetrievalEvaluationRunner: Send + Sync {
    async fn retrieve(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RetrievedEvidence>, RetrievalEvaluationError>;
}

pub fn score_retrieval_case(
    case: &RetrievalEvaluationCase,
    hits: &[RetrievedEvidence],
    limit: usize,
) -> Result<RetrievalCaseScore, RetrievalEvaluationError> {
    if limit == 0
        || case.expected.is_empty()
        || case.query.trim().is_empty()
        || case.expected.iter().any(|e| {
            e.span.start >= e.span.end || e.document_id.is_empty() || e.span.block_id.is_empty()
        })
    {
        return Err(RetrievalEvaluationError(
            "evaluation requires a query and original evidence spans".into(),
        ));
    }
    let mut targets = case.expected.clone();
    targets.sort_by_key(|e| {
        (
            e.document_id.clone(),
            e.span.block_id.clone(),
            e.span.start,
            e.span.end,
        )
    });
    targets.dedup();
    let mut covered = vec![Vec::<(usize, usize)>::new(); targets.len()];
    let mut matched = vec![false; targets.len()];
    let mut relevant_hits = 0;
    let mut reciprocal_rank = 0.0;
    let mut dcg = 0.0;
    for (rank, hit) in hits.iter().take(limit).enumerate() {
        let mut relevant = false;
        let mut new_matches = 0;
        for (i, target) in targets.iter().enumerate() {
            if target.document_id != hit.document_id {
                continue;
            }
            for span in &hit.spans {
                if span.block_id != target.span.block_id {
                    continue;
                }
                let start = span.start.max(target.span.start);
                let end = span.end.min(target.span.end);
                if start < end {
                    covered[i].push((start, end));
                    relevant = true;
                }
            }
            covered[i].sort_unstable();
            let mut total = 0;
            let mut end = 0;
            for &(start, finish) in &covered[i] {
                total += finish.saturating_sub(start.max(end));
                end = end.max(finish);
            }
            if total >= target.span.end - target.span.start && !matched[i] {
                matched[i] = true;
                new_matches += 1;
            }
        }
        if relevant {
            relevant_hits += 1;
        }
        if new_matches > 0 {
            if reciprocal_rank == 0.0 {
                reciprocal_rank = 1.0 / (rank + 1) as f64;
            }
            dcg += new_matches as f64 / ((rank + 2) as f64).log2();
        }
    }
    let matched_targets = matched.iter().filter(|&&yes| yes).count();
    let ideal = targets.len() as f64;
    Ok(RetrievalCaseScore {
        case_id: case.id.clone(),
        recall: matched_targets as f64 / targets.len() as f64,
        precision: relevant_hits as f64 / hits.len().min(limit).max(1) as f64,
        reciprocal_rank,
        discounted_recall: if ideal == 0.0 { 0.0 } else { dcg / ideal },
        matched_targets,
        total_targets: targets.len(),
        missing: targets
            .into_iter()
            .zip(matched)
            .filter_map(|(target, matched)| (!matched).then_some(target))
            .collect(),
    })
}

pub async fn evaluate_retrieval(
    runner: &dyn RetrievalEvaluationRunner,
    cases: &[RetrievalEvaluationCase],
    partition: EvaluationPartition,
    limit: usize,
) -> Result<(RetrievalEvaluationMetrics, Vec<RetrievalCaseScore>), RetrievalEvaluationError> {
    let selected: Vec<_> = cases
        .iter()
        .filter(|case| case.partition == partition)
        .collect();
    if selected.is_empty() {
        return Err(RetrievalEvaluationError(
            "evaluation partition is empty".into(),
        ));
    }
    let mut metrics = RetrievalEvaluationMetrics::default();
    let mut scores = Vec::new();
    for case in selected {
        let hits = runner.retrieve(&case.query, limit).await?;
        let score = score_retrieval_case(case, &hits, limit)?;
        metrics.cases += 1;
        metrics.recall += score.recall;
        metrics.precision += score.precision;
        metrics.mrr += score.reciprocal_rank;
        metrics.discounted_recall += score.discounted_recall;
        metrics.hit_rate += f64::from(score.matched_targets > 0);
        match case.provenance {
            EvaluationProvenance::HumanLabeled => metrics.human_labeled_cases += 1,
            EvaluationProvenance::Generated => metrics.generated_cases += 1,
        }
        scores.push(score);
    }
    let n = metrics.cases as f64;
    metrics.recall /= n;
    metrics.precision /= n;
    metrics.mrr /= n;
    metrics.discounted_recall /= n;
    metrics.hit_rate /= n;
    Ok((metrics, scores))
}

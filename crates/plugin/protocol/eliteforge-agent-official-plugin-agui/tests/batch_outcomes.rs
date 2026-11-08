use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_agui::{AgUiPlugin, AgUiStream, Event};
use eliteforge_agent_official_plugin_tools_discovery::ToolDiscoveryPlugin;
use eliteforge_agent_test_support::{TestBuilder, batch_outcomes::*};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[tokio::test]
async fn runtime_batch_outcomes_match_live_history_and_transcript() {
    let mut fixtures = serde_json::Map::new();
    for scenario in [
        BatchOutcomeScenario::ParallelCancellation,
        BatchOutcomeScenario::ExposeFailure,
        BatchOutcomeScenario::SetExposureFailure,
    ] {
        let agent = TestBuilder::default()
            .plugin(ChatModelPlugin::new(BatchOutcomeModel))
            .plugin(AgUiPlugin::default())
            .plugin(
                ToolDiscoveryPlugin::new()
                    .provider(StaticToolProvider::new(vec![scenario.binding()])),
            )
            .build()
            .await
            .unwrap();
        let mut request = RunRequest::new(scenario.name(), "run");
        request.limits.max_parallel_tools = 2;
        if scenario == BatchOutcomeScenario::ParallelCancellation {
            request
                .extensions
                .insert(ToolResultProcessorService(Arc::new(CancelAfterKnownResult)));
        }
        let (handle, mut stream) = agent.start_with_output::<AgUiStream>(request).unwrap();
        let (result, events) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(agent.wait(handle), async {
                let mut events = Vec::new();
                while let Some(event) = stream.recv().await {
                    events.push(event);
                }
                events
            })
        })
        .await
        .expect("parallel completion must not wait for the earlier pending sibling");
        if scenario == BatchOutcomeScenario::ParallelCancellation {
            assert!(matches!(
                result.unwrap_err().primary(),
                AgentError::Cancelled
            ));
        } else {
            result.unwrap();
        }
        let checkpoint = agent
            .framework()
            .services()
            .unwrap()
            .get::<StateStoreService>()
            .unwrap()
            .0
            .latest(scenario.name())
            .await
            .unwrap()
            .unwrap();
        let parent = checkpoint
            .history
            .tool_invocations
            .iter()
            .find(|r| r.batch.is_none())
            .unwrap();
        let results: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::ToolCallResult(r) => Some(r),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 1, "one aggregate result per parent");
        assert_eq!(results[0].tool_call_id.to_string(), parent.id.to_string());
        let live: Value = serde_json::from_str(&results[0].content).unwrap();
        assert_eq!(Some(&live["structured"]), parent.structured.as_ref());
        assert_eq!(live["isError"].as_bool(), parent.is_error);
        let batch: ToolBatchResult = serde_json::from_value(live["structured"].clone()).unwrap();
        let expected = if scenario == BatchOutcomeScenario::ParallelCancellation {
            vec![
                ToolBatchStatus::Unknown,
                ToolBatchStatus::Completed,
                ToolBatchStatus::NotExecuted,
            ]
        } else {
            vec![ToolBatchStatus::Failed; 3]
        };
        assert_eq!(
            batch.results.iter().map(|r| r.status).collect::<Vec<_>>(),
            expected
        );
        for row in &batch.results {
            let child = checkpoint
                .history
                .tool_invocations
                .iter()
                .find(|r| r.id == row.identity.invocation_id)
                .unwrap();
            assert_eq!(child.batch.as_ref(), Some(&row.identity));
            assert_eq!(child.is_error, row.is_error);
            if matches!(
                row.status,
                ToolBatchStatus::Completed | ToolBatchStatus::Failed
            ) {
                assert_eq!(
                    child
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            HistoryContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>(),
                    row.content.text()
                );
            }
        }
        if scenario == BatchOutcomeScenario::ParallelCancellation {
            assert_eq!(batch.results[1].content.text(), "confirmed success");
            assert!(
                checkpoint
                    .history
                    .tool_invocations
                    .iter()
                    .find(|r| r.batch.as_ref().is_some_and(|b| b.index == 2))
                    .unwrap()
                    .started_at_ms
                    .is_none()
            );
        } else {
            let model_result = checkpoint
                .transcript
                .iter()
                .find(|m| m.role == Role::Tool)
                .unwrap();
            let compact: Value = serde_json::from_str(&model_result.content.text()).unwrap();
            assert!(
                compact["results"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|r| r["isError"] == true)
            );
            let finished: Vec<_> = checkpoint
                .events
                .iter()
                .filter_map(|e| match &e.event {
                    AgentEvent::ToolFinished { is_error, .. } => Some(*is_error),
                    _ => None,
                })
                .collect();
            assert_eq!(finished, vec![true; 3]);
        }
        fixtures.insert(scenario.name().into(), outcome_snapshot(live, parent));
        assert!(agent.framework().close().await.is_success());
    }
    let fixture = Value::Object(fixtures);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../contracts/batch-outcomes.json");
    if std::env::var_os("UPDATE_BATCH_OUTCOMES_FIXTURE").is_some() {
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap()),
        )
        .unwrap();
    }
    let expected: Value = serde_json::from_slice(
        &std::fs::read(path).expect("generate with UPDATE_BATCH_OUTCOMES_FIXTURE=1"),
    )
    .unwrap();
    assert_eq!(
        fixture, expected,
        "runtime output changed; rerun downstream consumers"
    );
    assert_ne!(fixture, json!({}));
}

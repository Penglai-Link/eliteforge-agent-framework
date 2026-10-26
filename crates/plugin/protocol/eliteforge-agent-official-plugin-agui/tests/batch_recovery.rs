use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_agui::{AgUiPlugin, AgUiStream, Event};
use eliteforge_agent_official_plugin_tools_discovery::ToolDiscoveryPlugin;
use eliteforge_agent_test_support::{TestBuilder, batch_outcomes::*};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

async fn run(agent: &Agent, request: RunRequest) -> (Result<RunResult, AgentError>, Vec<Event>) {
    let (handle, mut stream) = agent.start_with_output::<AgUiStream>(request).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(agent.wait(handle), async {
            let mut events = Vec::new();
            while let Some(event) = stream.recv().await {
                events.push(event);
            }
            events
        })
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn failed_processing_of_an_approval_answer_still_commits_recovery_context() {
    let model = BatchRecoveryModel::default();
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(AgUiPlugin::default())
        .plugin(ToolPolicyPlugin::new("mixed-approval", MixedBatchApproval))
        .plugin(
            ToolDiscoveryPlugin::new().provider(StaticToolProvider::new(vec![
                BatchOutcomeScenario::MixedApproval.binding(),
            ])),
        )
        .build()
        .await
        .unwrap();
    let store = agent
        .framework()
        .services()
        .unwrap()
        .get::<StateStoreService>()
        .unwrap()
        .0
        .clone();
    let (paused, _) = run(&agent, RunRequest::new("answer-failure", "go")).await;
    let paused = paused.unwrap();
    let cp = store.latest("answer-failure").await.unwrap().unwrap();
    let processor = FailSecondResult::default();
    let mut request = RunRequest::new("answer-failure", "");
    request.resume = Some(ResumeRequest {
        run_id: cp.run_id,
        response: Some(InteractionResponse {
            interaction_id: paused.interactions[0].id,
            value: json!(false),
        }),
    });
    request
        .extensions
        .insert(ToolResultProcessorService(Arc::new(processor.clone())));
    let (result, events) = run(&agent, request).await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("host audit unavailable")
    );
    let cp = store.latest("answer-failure").await.unwrap().unwrap();
    assert_eq!(cp.status, ExecutionStatus::Failed);
    assert!(
        agent
            .pending_interactions("answer-failure")
            .await
            .unwrap()
            .is_empty()
    );
    // The skipped approval remains an unanswered audit record, never a fake denial.
    let unused = agent
        .interaction("answer-failure", paused.interactions[1].id)
        .await
        .unwrap();
    assert!(unused.response.is_none());
    let interactions = agent
        .framework()
        .services()
        .unwrap()
        .get::<InteractionStoreService>()
        .unwrap();
    assert!(
        interactions
            .0
            .answer(
                "answer-failure",
                &InteractionResponse {
                    interaction_id: unused.request.id,
                    value: json!(true),
                }
            )
            .await
            .is_err()
    );
    interactions.0.publish(&unused).await.unwrap();
    assert!(
        agent
            .pending_interactions("answer-failure")
            .await
            .unwrap()
            .is_empty(),
        "late publication cannot reopen a terminal run"
    );
    assert_eq!(*processor.0.lock().unwrap(), vec![1]);
    let results: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::ToolCallResult(result) => {
                Some(serde_json::from_str::<Value>(&result.content).unwrap())
            }
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 1);
    assert_eq!(
        batch_statuses(&results[0]),
        ["Failed", "Failed", "NotExecuted"]
    );
    let parent = cp
        .history
        .tool_invocations
        .iter()
        .find(|r| r.batch.is_none())
        .unwrap();
    assert_eq!(Some(&results[0]["structured"]), parent.structured.as_ref());
    run(&agent, RunRequest::new("answer-failure", "continue"))
        .await
        .0
        .unwrap();
    assert_eq!(
        recovery_model_result(&model),
        serde_json::from_str::<Value>(results[0]["content"][0]["text"].as_str().unwrap()).unwrap()
    );
    assert!(agent.framework().close().await.is_success());
}

#[tokio::test]
async fn approved_batch_streams_committed_running_and_partial_results() {
    let control = BatchProgressControl::default();
    let model = BatchRecoveryModel::default();
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(AgUiPlugin::default())
        .plugin(ToolPolicyPlugin::new(
            "progress-approval",
            BatchProgressApproval,
        ))
        .plugin(
            ToolDiscoveryPlugin::new().provider(StaticToolProvider::new(vec![control.binding()])),
        )
        .build()
        .await
        .unwrap();
    let store = agent
        .framework()
        .services()
        .unwrap()
        .get::<StateStoreService>()
        .unwrap()
        .0
        .clone();
    let mut request = RunRequest::new("progress", "go");
    request.limits.max_parallel_tools = 2;
    let (paused, _) = run(&agent, request).await;
    let paused = paused.unwrap();
    let cp = store.latest("progress").await.unwrap().unwrap();
    let mut request = RunRequest::new("progress", "");
    request.resume = Some(ResumeRequest {
        run_id: cp.run_id,
        response: Some(InteractionResponse {
            interaction_id: paused.interactions[0].id,
            value: json!(true),
        }),
    });
    let (handle, mut stream) = agent.start_with_output::<AgUiStream>(request).unwrap();
    let mut fixtures = serde_json::Map::new();
    let mut last_revision = cp
        .history
        .tool_invocations
        .iter()
        .find(|r| r.batch.is_none())
        .unwrap()
        .batch_revision
        .as_ref()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    for (name, expected, release) in [
        ("approvedRunning", ["Failed", "Running", "Running"], 1),
        ("partialCompleted", ["Failed", "Completed", "Running"], 2),
    ] {
        let snapshot = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = stream.recv().await {
                if let Event::ActivitySnapshot(snapshot) = event
                    && snapshot.activity_type == TOOL_BATCH_ACTIVITY_TYPE
                {
                    let value = serde_json::to_value(snapshot.content).unwrap();
                    let revision = value["revision"].as_str().unwrap().parse::<u64>().unwrap();
                    assert!(
                        revision >= last_revision,
                        "resume/replay cannot move a batch revision backwards"
                    );
                    last_revision = revision;
                    if batch_statuses(&value["output"]) == expected {
                        return value;
                    }
                }
            }
            panic!("missing {name} snapshot");
        })
        .await
        .unwrap();
        let cp = store.latest("progress").await.unwrap().unwrap();
        fixtures.insert(
            name.into(),
            committed_progress_snapshot(&cp, snapshot, &expected),
        );
        control.release(release);
    }
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
    .unwrap();
    result.unwrap();
    let cp = store.latest("progress").await.unwrap().unwrap();
    let parent = cp
        .history
        .tool_invocations
        .iter()
        .find(|r| r.batch.is_none())
        .unwrap();
    let live = events
        .iter()
        .find_map(|event| match event {
            Event::ToolCallResult(result) => {
                Some(serde_json::from_str::<Value>(&result.content).unwrap())
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(Some(&live["structured"]), parent.structured.as_ref());
    assert!(
        parent
            .batch_revision
            .as_ref()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > last_revision
    );
    let mut final_value = recovery_snapshot(live.clone(), parent, None);
    final_value["model"] = recovery_model_result(&model);
    assert_eq!(
        final_value["model"],
        serde_json::from_str::<Value>(live["content"][0]["text"].as_str().unwrap()).unwrap()
    );
    fixtures.insert("progressCompleted".into(), final_value);
    assert!(agent.framework().close().await.is_success());
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../contracts/batch-progress.json");
    let actual = Value::Object(fixtures);
    if std::env::var_os("UPDATE_BATCH_RECOVERY_FIXTURE").is_some() {
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&actual).unwrap()),
        )
        .unwrap();
    }
    assert_eq!(
        actual,
        serde_json::from_slice::<Value>(&std::fs::read(path).unwrap()).unwrap()
    );
}

#[tokio::test]
async fn batch_recovery_context_and_snapshots_match_history() {
    let mut fixtures = serde_json::Map::new();
    for scenario in [
        BatchOutcomeScenario::ParallelCancellation,
        BatchOutcomeScenario::MixedApproval,
        BatchOutcomeScenario::ProcessorFailure,
    ] {
        let model = BatchRecoveryModel::default();
        let mut builder = TestBuilder::default()
            .plugin(ChatModelPlugin::new(model.clone()))
            .plugin(AgUiPlugin::default())
            .plugin(
                ToolDiscoveryPlugin::new()
                    .provider(StaticToolProvider::new(vec![scenario.binding()])),
            );
        if scenario == BatchOutcomeScenario::MixedApproval {
            builder = builder.plugin(ToolPolicyPlugin::new("mixed-approval", MixedBatchApproval));
        }
        let agent = builder.build().await.unwrap();
        let store = agent
            .framework()
            .services()
            .unwrap()
            .get::<StateStoreService>()
            .unwrap()
            .0
            .clone();
        let mut request = RunRequest::new(scenario.name(), "run");
        request.limits.max_parallel_tools = 2;
        if scenario == BatchOutcomeScenario::ParallelCancellation {
            request
                .extensions
                .insert(ToolResultProcessorService(Arc::new(CancelAfterKnownResult)));
        }
        let processor = FailSecondResult::default();
        if scenario == BatchOutcomeScenario::ProcessorFailure {
            request
                .extensions
                .insert(ToolResultProcessorService(Arc::new(processor.clone())));
        }
        let mut pending = Vec::new();
        let mut logical_run = None;
        let phases: &[&str] = if scenario == BatchOutcomeScenario::ParallelCancellation {
            &["cancelContinue"]
        } else if scenario == BatchOutcomeScenario::ProcessorFailure {
            &["processorFailure"]
        } else {
            &["mixedWaiting", "mixedRejected", "mixedCompleted"]
        };
        for (phase, name) in phases.iter().enumerate() {
            if phase > 0 {
                request = RunRequest::new(scenario.name(), "");
                request.resume = Some(ResumeRequest {
                    run_id: logical_run.unwrap(),
                    response: Some(InteractionResponse {
                        interaction_id: pending[phase - 1],
                        value: json!(phase == 2),
                    }),
                });
            }
            let (result, events) = run(&agent, request.clone()).await;
            if scenario == BatchOutcomeScenario::ParallelCancellation {
                assert!(matches!(
                    result.unwrap_err().primary(),
                    AgentError::Cancelled
                ));
            } else if scenario == BatchOutcomeScenario::ProcessorFailure {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("host audit unavailable")
                );
                let processed = processor.0.lock().unwrap().clone();
                assert_eq!(
                    processed.iter().filter(|&&i| i == 1).count(),
                    1,
                    "do not retry failed host processing"
                );
            } else {
                let result = result.unwrap();
                if phase == 0 {
                    pending = result.interactions.iter().map(|i| i.id).collect();
                }
                assert_eq!(result.interactions.len(), 2 - phase);
            }
            let cp = store.latest(scenario.name()).await.unwrap().unwrap();
            logical_run = Some(cp.run_id);
            let parent = cp
                .history
                .tool_invocations
                .iter()
                .find(|r| r.batch.is_none())
                .unwrap();
            let snapshots: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    Event::ActivitySnapshot(s) if s.activity_type == TOOL_BATCH_ACTIVITY_TYPE => {
                        Some(serde_json::to_value(&s.content).unwrap())
                    }
                    _ => None,
                })
                .collect();
            let results: Vec<_> = events
                .iter()
                .filter_map(|e| match e {
                    Event::ToolCallResult(r) => {
                        Some(serde_json::from_str::<Value>(&r.content).unwrap())
                    }
                    _ => None,
                })
                .collect();
            let waiting = scenario == BatchOutcomeScenario::MixedApproval && phase < 2;
            let (live, snapshot) = if waiting {
                assert!(results.is_empty(), "pause must not finalize the parent");
                let snapshot = snapshots.last().expect("latest committed snapshot");
                assert_eq!(snapshot["toolCallId"], parent.id.to_string());
                assert_eq!(parent.status, HistoryStatus::Waiting);
                (snapshot["output"].clone(), Some(snapshot.clone()))
            } else {
                assert_eq!(results.len(), 1);
                (results[0].clone(), None)
            };
            assert_eq!(live["structured"], parent.structured.clone().unwrap());
            let statuses: Vec<_> = live["structured"]["results"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["status"].as_str().unwrap())
                .collect();
            assert_eq!(
                statuses,
                match *name {
                    "cancelContinue" => vec!["Unknown", "Completed", "NotExecuted"],
                    "processorFailure" => vec!["Completed", "Failed", "Completed"],
                    "mixedWaiting" => vec!["Failed", "Waiting", "Waiting"],
                    "mixedRejected" => vec!["Failed", "Failed", "Waiting"],
                    "mixedCompleted" => vec!["Failed", "Failed", "Completed"],
                    _ => unreachable!(),
                }
            );
            let mut snapshot = recovery_snapshot(live.clone(), parent, snapshot);
            if !waiting {
                if scenario != BatchOutcomeScenario::MixedApproval {
                    assert!(cp.status.is_terminal());
                    // A new user turn must carry the interrupted provider pair,
                    // without resuming or redispatching the cancelled cursor.
                    run(&agent, RunRequest::new(scenario.name(), "continue"))
                        .await
                        .0
                        .unwrap();
                }
                let compact = recovery_model_result(&model);
                let live_compact: Value =
                    serde_json::from_str(live["content"][0]["text"].as_str().unwrap()).unwrap();
                assert_eq!(compact, live_compact);
                snapshot["model"] = compact;
            } else {
                assert!(
                    !cp.transcript.iter().any(|m| m.role == Role::Tool),
                    "no partial provider result during approval"
                );
            }
            fixtures.insert((*name).into(), snapshot);
        }
        assert!(agent.framework().close().await.is_success());
    }
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../contracts/batch-recovery.json");
    let actual = Value::Object(fixtures);
    if std::env::var_os("UPDATE_BATCH_RECOVERY_FIXTURE").is_some() {
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&actual).unwrap()),
        )
        .unwrap();
    }
    let expected: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(actual, expected);
}

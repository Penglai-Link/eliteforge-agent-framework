use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_agui::{AgUiPlugin, AgUiStream, Event};
use eliteforge_agent_official_plugin_tools_discovery::ToolDiscoveryPlugin;
use eliteforge_agent_test_support::{
    TestBuilder, batch_outcomes::outcome_snapshot, tool_search::*,
};
use serde_json::{Value, json};

#[tokio::test]
async fn two_stage_search_matches_model_stream_and_persistent_history() {
    let model = TwoStageSearchModel::default();
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(model.clone()))
        .plugin(AgUiPlugin::default())
        .plugin(ToolDiscoveryPlugin::new().provider(StaticToolProvider::new(two_stage_bindings())))
        .build()
        .await
        .unwrap();
    let (handle, mut stream) = agent
        .start_with_output::<AgUiStream>(RunRequest::new("search-contract", "inspect"))
        .unwrap();
    let (result, events) = tokio::join!(agent.wait(handle), async {
        let mut events = Vec::new();
        while let Some(event) = stream.recv().await {
            events.push(event);
        }
        events
    });
    result.unwrap();
    assert_two_stage_requests(&model);
    let cp = agent
        .framework()
        .services()
        .unwrap()
        .get::<StateStoreService>()
        .unwrap()
        .0
        .latest("search-contract")
        .await
        .unwrap()
        .unwrap();
    let mut fixture = json!({});
    for event in events {
        if let Event::ToolCallResult(result) = event {
            let parent = cp
                .history
                .tool_invocations
                .iter()
                .find(|r| r.id.to_string() == result.tool_call_id.to_string())
                .unwrap();
            let live: Value = serde_json::from_str(&result.content).unwrap();
            assert_eq!(Some(&live["structured"]), parent.structured.as_ref());
            fixture[parent.call_id.clone()] = outcome_snapshot(live, parent);
        }
    }
    assert_eq!(fixture.as_object().unwrap().len(), 3);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../contracts/tool-search.json");
    if std::env::var_os("UPDATE_TOOL_SEARCH_FIXTURE").is_some() {
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap()),
        )
        .unwrap();
    }
    let expected: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(fixture, expected);
    assert!(agent.framework().close().await.is_success());
}

use eliteforge_agent_core::*;
use serde_json::json;

fn identity(index: usize) -> ToolBatchItem {
    ToolBatchItem {
        parent_call_id: "provider-batch".into(),
        parent_invocation_id: RunId::from_u128(1),
        invocation_id: RunId::from_u128(index as u128 + 2),
        call_id: format!("child-{index}"),
        index,
    }
}
fn public_output(mut output: ToolOutput) -> serde_json::Value {
    // serde_json's preserve_order feature can be enabled by another workspace
    // crate. Key order inside JSON text is not part of the wire contract.
    if let ContentBlock::Text { text } = &mut output.content.0[0] {
        let mut compact: serde_json::Value = serde_json::from_str(text).unwrap();
        compact.sort_all_objects();
        *text = compact.to_string();
    }
    json!({"content":output.content,"structured":output.structured,"isError":output.is_error,"resultReference":output.result_reference})
}
#[test]
fn batch_contract_fixture_matches_framework_serialization() {
    let mut successful = ToolOutput::text(Content(vec![
        ContentBlock::Text {
            text: "Generated image and source evidence".into(),
        },
        ContentBlock::Json {
            value: json!({"uri":"fixture:doc","text":"embedded source data"}),
        },
        ContentBlock::Media {
            media: MediaContent {
                mime_type: "image/png".into(),
                source: MediaSource::Base64("YWJj".into()),
                name: Some("local preview".into()),
                estimated_tokens: Some(10),
            },
        },
    ]));
    successful.structured = Some(
        json!({"task_id":"image-task","job":{"status":"Completed","progress":100,"images":[{"url":"https://example.com/image.png"}]},"results":[{"url":"https://example.com/source","title":"Source","snippet":"Evidence"}]}),
    );
    successful.result_reference = Some(Box::new(ResourceReference {
        uri: "https://example.com/download".into(),
        mime_type: Some("text/plain".into()),
        name: Some("Download".into()),
        size_bytes: Some(12),
    }));
    let mut failed = ToolOutput::error("read failed: permission denied");
    failed.structured = Some(json!({"code":"permission-denied"}));
    let complete = tool_batch_output([
        ToolBatchItemResult::finished(identity(0), &successful),
        ToolBatchItemResult::finished(identity(1), &failed),
    ]);
    let interrupted = tool_batch_output([
        ToolBatchItemResult::finished(identity(0), &successful),
        ToolBatchItemResult::pending(identity(1), ToolBatchStatus::Unknown),
        ToolBatchItemResult::pending(identity(2), ToolBatchStatus::NotExecuted),
    ]);
    let approval = ToolApprovalDetails {
        tool: "write_record".into(),
        parameters: json!({"name":"second record"}),
        batch: Some(identity(1)),
    };
    let fixture = json!({"complete":public_output(complete),"interrupted":public_output(interrupted),"approval":approval});
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../contracts/tool-batch.json");
    if std::env::var_os("UPDATE_TOOL_BATCH_FIXTURE").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&fixture).unwrap()),
        )
        .unwrap();
    }
    let expected: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(path)
            .expect("generate the contract fixture with UPDATE_TOOL_BATCH_FIXTURE=1"),
    )
    .unwrap();
    assert_eq!(
        fixture, expected,
        "Rust wire contract changed; regenerate and run UI/Tron consumer tests"
    );
    let batch: ToolBatchResult =
        serde_json::from_value(fixture["interrupted"]["structured"].clone()).unwrap();
    assert_eq!(batch.results[1].status, ToolBatchStatus::Unknown);
    assert_eq!(batch.results[2].status, ToolBatchStatus::NotExecuted);
    assert!(
        !fixture["complete"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("YWJj")
    );
}

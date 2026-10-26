//! Batch-only model contracts over atomic provider implementations.
use crate::{
    Content, ContentBlock, ResourceReference, RunId, ToolDescriptor, ToolInputMode, ToolOutput,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const MAX_TOOL_BATCH_ITEMS: usize = 32;

/// Shared identity for execution, approval, history and presentation. Provider
/// pairing uses parent_call_id; UI pairing uses parent_invocation_id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolBatchItem {
    pub invocation_id: RunId,
    pub call_id: String,
    pub parent_call_id: String,
    pub parent_invocation_id: RunId,
    pub index: usize,
}

/// Project only at the model boundary. Policies and executors keep the original
/// descriptor and receive one item's arguments, never the batch wrapper.
pub fn batch_tool_descriptor(descriptor: &ToolDescriptor) -> ToolDescriptor {
    if descriptor.input_mode == ToolInputMode::Single {
        return descriptor.clone();
    }
    let mut result = descriptor.clone();
    let mut item_schema = descriptor.input_schema.clone();
    rebase_local_refs(&mut item_schema);
    result.input_schema = json!({
        "type": "object", "additionalProperties": false, "required": ["items"],
        "properties": {"items": {
            "type": "array", "minItems": 1, "maxItems": MAX_TOOL_BATCH_ITEMS,
            "items": item_schema
        }}
    });
    result.description = format!(
        "Batch operation: combine all known independent requests in items (1–{MAX_TOOL_BATCH_ITEMS}); one request also requires an array. Results are in input order with per-item errors. Do not batch requests that depend on earlier results. {}",
        descriptor.description
    );
    result
}

// A schema with its own resource ID retains its local reference scope. Without
// one, fragment references must point to the schema's new location in the wrapper.
fn rebase_local_refs(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if map.contains_key("$id") {
                return;
            }
            for (key, value) in map {
                if matches!(key.as_str(), "$ref" | "$dynamicRef") {
                    if let Some(reference) = value.as_str() {
                        if reference == "#" || reference.starts_with("#/") {
                            *value = Value::String(format!(
                                "#/properties/items/items{}",
                                &reference[1..]
                            ));
                        }
                    }
                } else if !matches!(key.as_str(), "default" | "examples" | "const" | "enum") {
                    rebase_local_refs(value);
                }
            }
        }
        Value::Array(values) => values.iter_mut().for_each(rebase_local_refs),
        _ => {}
    }
}

/// Validate the wrapper separately so an invalid item does not discard valid
/// siblings. The runtime validates each item against the original schema.
pub fn tool_batch_items(arguments: &Value) -> Result<&[Value], String> {
    let map = arguments
        .as_object()
        .ok_or("Expected an object with items")?;
    if map.len() != 1 {
        return Err("Batch tools accept only {\"items\":[...]}".into());
    }
    let items = map
        .get("items")
        .and_then(Value::as_array)
        .ok_or("Batch tools require an items array, including for one item")?;
    if items.is_empty() || items.len() > MAX_TOOL_BATCH_ITEMS {
        return Err(format!(
            "items must contain 1–{MAX_TOOL_BATCH_ITEMS} requests"
        ));
    }
    Ok(items)
}

/// Per-item lifecycle. A stopped run distinguishes undispatched work from
/// started work whose outcome cannot be confirmed.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToolBatchStatus {
    Pending,
    Waiting,
    Running,
    Completed,
    Failed,
    NotExecuted,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolBatchItemResult {
    #[serde(flatten)]
    pub identity: ToolBatchItem,
    pub status: ToolBatchStatus,
    /// Original content is retained independently of the structured payload.
    pub content: Content,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured: Option<Value>,
    /// Compact convenience value for model consumers; content remains authoritative.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_reference: Option<ResourceReference>,
}
impl ToolBatchItemResult {
    pub fn pending(identity: ToolBatchItem, status: ToolBatchStatus) -> Self {
        Self {
            identity,
            status,
            content: Content::default(),
            structured: None,
            result: None,
            is_error: None,
            result_reference: None,
        }
    }
    pub fn finished(identity: ToolBatchItem, output: &ToolOutput) -> Self {
        let text = Content(
            output
                .content
                .0
                .iter()
                .filter(|b| matches!(b, ContentBlock::Text { .. } | ContentBlock::Json { .. }))
                .cloned()
                .collect(),
        );
        let result = output.structured.clone().unwrap_or_else(|| {
            if text
                .0
                .iter()
                .all(|b| matches!(b, ContentBlock::Text { .. }))
            {
                Value::String(text.text())
            } else {
                serde_json::to_value(text).expect("content serializes")
            }
        });
        Self {
            identity,
            status: if output.is_error {
                ToolBatchStatus::Failed
            } else {
                ToolBatchStatus::Completed
            },
            content: output.content.clone(),
            structured: output.structured.clone(),
            result: Some(result),
            is_error: Some(output.is_error),
            result_reference: output.result_reference.as_deref().cloned(),
        }
    }
}

/// Public wire contract, used unchanged by live results and durable history.
/// A discriminator prevents confusing a search tool's own `results` with a batch.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolBatchResult {
    pub kind: ToolBatchResultKind,
    pub results: Vec<ToolBatchItemResult>,
}

/// Replaceable progress, never a final tool result. The call ID is the parent's
/// invocation identity (the same identity used by live cards and history).
pub const TOOL_BATCH_ACTIVITY_TYPE: &str = "framework.tool.batch";
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolBatchSnapshot {
    /// Parent-local durable revision; compare only within the same invocation.
    pub revision: String,
    pub tool_call_id: RunId,
    pub tool_name: String,
    pub arguments: Value,
    pub output: ToolOutput,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ToolBatchResultKind {
    #[serde(rename = "toolBatch")]
    ToolBatch,
}
impl ToolBatchResult {
    pub fn new(items: impl IntoIterator<Item = ToolBatchItemResult>) -> Self {
        let mut results: Vec<_> = items.into_iter().collect();
        results.sort_by_key(|r| r.identity.index);
        Self {
            kind: ToolBatchResultKind::ToolBatch,
            results,
        }
    }
}

/// Approval presentation uses the same identity as batch results and history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolApprovalDetails {
    pub tool: String,
    pub parameters: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch: Option<ToolBatchItem>,
}

/// Full wire data for applications, compact content for the model. Only proven
/// duplicate text/JSON is omitted from model text; typed media stays typed.
pub fn tool_batch_output(items: impl IntoIterator<Item = ToolBatchItemResult>) -> ToolOutput {
    let batch = ToolBatchResult::new(items);
    let mut attachments = Vec::new();
    let rows: Vec<_> = batch
        .results
        .iter()
        .map(|item| {
            let mut row = json!({"index":item.identity.index,"status":item.status});
            if let Some(error) = item.is_error {
                row["isError"] = json!(error);
            }
            if let Some(result) = &item.result {
                row["result"] = result.clone();
            }
            if let Some(reference) = &item.result_reference {
                row["resultReference"] = json!(reference);
            }
            let mut extra = Vec::new();
            let mut media = Vec::new();
            // No content is dropped from the public contract. For model content,
            // compare complete values before deduplication, never merely presence.
            for block in &item.content.0 {
                match block {
                    ContentBlock::Media { .. } | ContentBlock::Resource { .. } => {
                        media.push(block.clone())
                    }
                    ContentBlock::Text { text }
                        if item.result.as_ref().is_some_and(|value| {
                            value.as_str() == Some(text.as_str())
                                || serde_json::from_str::<Value>(text).ok().as_ref() == Some(value)
                        }) => {}
                    ContentBlock::Json { value } if item.result.as_ref() == Some(value) => {}
                    _ => extra.push(block.clone()),
                }
            }
            // An unstructured multi-block result already contains the complete text/JSON.
            if item.structured.is_some() && !extra.is_empty() {
                row["content"] = json!(extra);
            }
            if !media.is_empty() {
                row["attachmentCount"] = json!(media.len());
                attachments.push(ContentBlock::Text {
                    text: format!("\nAttachments for batch item {}:\n", item.identity.index),
                });
                attachments.extend(media);
            }
            row
        })
        .collect();
    let mut output = ToolOutput::text(json!({"results":rows}).to_string());
    output.content.0.extend(attachments);
    output.is_error = batch.results.iter().any(|r| {
        r.is_error == Some(true)
            || matches!(
                r.status,
                ToolBatchStatus::Unknown | ToolBatchStatus::NotExecuted
            )
    });
    output.structured = Some(serde_json::to_value(batch).expect("batch serializes"));
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn batch_schema_preserves_local_references_and_rejects_single_input() {
        let d = ToolDescriptor::new(
            "read",
            "read",
            "Read",
            json!({
                "type":"object", "required":["path"], "additionalProperties":false,
                "properties":{"path":{"$ref":"#/$defs/path"}},
                "$defs":{"path":{"type":"string","minLength":1}}
            }),
        );
        let projected = batch_tool_descriptor(&d);
        let schema = crate::compile_schema(&projected.input_schema).unwrap();
        assert!(schema.is_valid(&json!({"items":[{"path":"a"},{"path":"b"}]})));
        assert!(!schema.is_valid(&json!({"path":"a"})));
        assert!(!schema.is_valid(&json!({"items":[{"path":3}]})));
        assert_eq!(
            batch_tool_descriptor(&d.clone().with_single_input()),
            d.with_single_input()
        );
    }
    #[test]
    fn batch_wrapper_is_bounded_but_invalid_items_are_independent() {
        for value in [
            json!({}),
            json!({"items":[]}),
            json!({"items":[{}],"extra":true}),
            json!({"items":vec![json!({});33]}),
        ] {
            assert!(tool_batch_items(&value).is_err());
        }
        assert_eq!(
            tool_batch_items(&json!({"items":[{},null,4]}))
                .unwrap()
                .len(),
            3
        );
    }
}

#[cfg(test)]
#[test]
fn batch_outputs_preserve_typed_media_without_serializing_pixels_into_text() {
    let media = crate::ContentBlock::Media {
        media: crate::MediaContent {
            mime_type: "image/png".into(),
            source: crate::MediaSource::Base64("pixel-payload".into()),
            name: None,
            estimated_tokens: Some(10),
        },
    };
    let mut item = ToolOutput::text(crate::Content(vec![media.clone()]));
    item.structured = Some(json!({"path":"image.png"}));
    let output = tool_batch_output([ToolBatchItemResult::finished(test_identity(0), &item)]);
    assert!(output.content.0.contains(&media));
    assert!(!output.content.text().contains("pixel-payload"));
    assert_eq!(
        output.structured.as_ref().unwrap()["results"][0]["content"][0]["type"],
        "media"
    );
}

#[cfg(test)]
fn test_identity(index: usize) -> ToolBatchItem {
    ToolBatchItem {
        parent_call_id: "batch".into(),
        parent_invocation_id: RunId::from_u128(1),
        invocation_id: RunId::from_u128(index as u128 + 2),
        call_id: format!("item-{index}"),
        index,
    }
}
#[cfg(test)]
#[test]
fn preserves_independent_mcp_text_json_and_structured_content() {
    let mut output = ToolOutput::text(Content(vec![
        ContentBlock::Text {
            text: "read failed".into(),
        },
        ContentBlock::Json {
            value: json!({"uri":"fixture:doc","text":"source data"}),
        },
    ]));
    output.structured = Some(json!({"code":"fixture-error"}));
    output.is_error = true;
    let batch = tool_batch_output([ToolBatchItemResult::finished(test_identity(0), &output)]);
    let parsed: ToolBatchResult = serde_json::from_value(batch.structured.unwrap()).unwrap();
    assert_eq!(parsed.results[0].content, output.content);
    assert_eq!(parsed.results[0].structured, output.structured);
    assert!(batch.content.text().contains("read failed"));
    assert!(batch.content.text().contains("source data"));
}

//! Capability-scoped artifact offloading and bounded reads.
//! The host supplies an ArtifactStore scoped to each Agent.

use std::{collections::HashSet, io::Write, sync::Arc};

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use eliteforge_agent_core::{
    Content, ContentBlock, ResourceReference, RunContext, Tool, ToolBinding, ToolDescriptor,
    ToolError, ToolInvocationContext, ToolMiddleware, ToolNext, ToolOutput,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const TOOL_OUTPUT_MIME: &str = "application/vnd.eliteforge-agent-framework.tool-output+json";

pub use eliteforge_agent_context_contracts::{
    ArtifactChunk, ArtifactError, ArtifactId, ArtifactMetadata, ArtifactStore, ArtifactStorePlugin,
    ArtifactStoreService,
};

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

fn check_cancelled(token: &CancellationToken) -> Result<(), ArtifactError> {
    if token.is_cancelled() {
        Err(ArtifactError::Cancelled)
    } else {
        Ok(())
    }
}

/// Offloads content and structured data. Error and control fields remain inline.
/// Add the artifact reader's tool ID to exclusions when its configured range
/// could itself exceed the offload threshold.
pub struct ToolOutputOffloader {
    store: Arc<dyn ArtifactStore>,
    threshold_bytes: usize,
    preview_bytes: usize,
    excluded_tools: HashSet<String>,
}

impl ToolOutputOffloader {
    pub fn new(
        store: Arc<dyn ArtifactStore>,
        threshold_bytes: usize,
        preview_bytes: usize,
    ) -> Result<Self, ArtifactError> {
        if threshold_bytes == 0 || preview_bytes > threshold_bytes {
            return Err(ArtifactError::InvalidLimits(
                "offload threshold must be positive and preview <= threshold",
            ));
        }
        Ok(Self {
            store,
            threshold_bytes,
            preview_bytes,
            excluded_tools: HashSet::new(),
        })
    }

    pub fn excluding_tool(mut self, tool_id: impl Into<String>) -> Self {
        self.excluded_tools.insert(tool_id.into());
        self
    }

    pub async fn offload(
        &self,
        output: &mut ToolOutput,
        cancellation: CancellationToken,
    ) -> Result<Option<ArtifactMetadata>, ArtifactError> {
        check_cancelled(&cancellation)?;
        let mut counter = BoundedBuffer::counting(self.threshold_bytes);
        if serde_json::to_writer(&mut counter, &(&output.content, &output.structured)).is_ok() {
            return Ok(None);
        }
        let mut buffer = BoundedBuffer::collecting(self.store.max_artifact_bytes());
        if let Err(error) = serde_json::to_writer(&mut buffer, &*output) {
            return if buffer.exceeded {
                Err(ArtifactError::TooLarge)
            } else {
                Err(ArtifactError::Serialization(error))
            };
        }
        let preview = content_preview(&output.content, self.preview_bytes);
        // Original content and runtime semantics remain unchanged on failure.
        let metadata = self.store.put(buffer.bytes, cancellation).await?;
        let reference = ResourceReference {
            uri: format!("artifact:{}", metadata.id),
            mime_type: Some(TOOL_OUTPUT_MIME.into()),
            name: Some("Original tool output".into()),
            size_bytes: Some(metadata.size_bytes),
        };
        output.structured = None;
        output.result_reference = Some(Box::new(reference.clone()));
        output.content = vec![
            ContentBlock::Text {
                text: format!(
                    "Tool output stored as artifact {} ({} bytes). Preview:\n{}",
                    metadata.id, metadata.size_bytes, preview
                ),
            },
            ContentBlock::Resource {
                resource: reference,
            },
        ]
        .into();
        Ok(Some(metadata))
    }
}

fn content_preview(content: &Content, limit: usize) -> String {
    let mut preview = String::new();
    for block in &content.0 {
        if let ContentBlock::Text { text } = block {
            let mut length = text.len().min(limit - preview.len());
            while !text.is_char_boundary(length) {
                length -= 1;
            }
            preview.push_str(&text[..length]);
            if length < text.len() || preview.len() == limit {
                break;
            }
        }
    }
    preview
}

struct BoundedBuffer {
    bytes: Vec<u8>,
    size: usize,
    limit: usize,
    collect: bool,
    exceeded: bool,
}
impl BoundedBuffer {
    fn counting(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            size: 0,
            limit,
            collect: false,
            exceeded: false,
        }
    }
    fn collecting(limit: usize) -> Self {
        Self {
            collect: true,
            ..Self::counting(limit)
        }
    }
}
impl Write for BoundedBuffer {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if data.len() > self.limit - self.size {
            self.exceeded = true;
            return Err(std::io::Error::other("artifact byte limit exceeded"));
        }
        self.size += data.len();
        if self.collect {
            self.bytes.extend_from_slice(data);
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl ToolMiddleware for ToolOutputOffloader {
    async fn intercept(
        &self,
        context: &ToolInvocationContext<'_>,
        next: ToolNext<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let mut output = next.run().await?;
        if self.excluded_tools.contains(&context.descriptor.id) {
            return Ok(output);
        }
        {
            let token = CancellationToken::new();
            let _cancel_on_drop = CancelOnDrop(token.clone());
            tokio::select! {
                biased;
                _ = context.run.cancelled() => {
                    token.cancel();
                    return Err(ToolError::Failed("artifact offload cancelled".into()));
                }
                result = self.offload(&mut output, token.clone()) => {
                    result.map_err(|e| ToolError::Failed(e.to_string()))?;
                }
            }
        }
        Ok(output)
    }
}

/// Reads raw artifact bytes in bounded pages. UTF-8 pages are returned as text;
/// pages splitting a codepoint or containing binary data use base64 losslessly.
#[derive(Clone)]
pub struct ArtifactReadTool {
    store: Arc<dyn ArtifactStore>,
}
impl ArtifactReadTool {
    pub fn new(store: Arc<dyn ArtifactStore>) -> Self {
        Self { store }
    }

    pub fn binding(self, id: impl Into<String>, call_name: impl Into<String>) -> ToolBinding {
        let max_read_bytes = self.store.max_read_bytes();
        ToolBinding::new(
            ToolDescriptor::new(
                id,
                call_name,
                "Read a bounded byte range from an authorized artifact. Use next_offset to continue; encoding is utf8 or base64.",
                json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "pattern": "^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$"},
                        "offset": {"type": "integer", "minimum": 0},
                        "length": {"type": "integer", "minimum": 1, "maximum": max_read_bytes}
                    },
                    "required": ["id", "length"],
                    "additionalProperties": false
                }),
            ).with_operation_kind(eliteforge_agent_core::ToolOperationKind::Query),
            self,
        )
        .parallel_safe()
    }

    async fn read_arguments(
        &self,
        arguments: Value,
        token: CancellationToken,
    ) -> Result<ToolOutput, ArtifactError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Arguments {
            id: ArtifactId,
            #[serde(default)]
            offset: u64,
            length: usize,
        }
        let arguments: Arguments = serde_json::from_value(arguments)?;
        let chunk = self
            .store
            .read(&arguments.id, arguments.offset, arguments.length, token)
            .await?;
        let (encoding, data) = match std::str::from_utf8(&chunk.bytes) {
            Ok(text) => ("utf8", text.to_owned()),
            Err(_) => ("base64", STANDARD.encode(&chunk.bytes)),
        };
        let value = json!({
            "id": chunk.metadata.id,
            "size_bytes": chunk.metadata.size_bytes,
            "offset": chunk.offset,
            "next_offset": chunk.next_offset(),
            "eof": chunk.eof(),
            "encoding": encoding,
            "data": data,
        });
        Ok(ToolOutput {
            structured: Some(value.clone()),
            content: Content(vec![ContentBlock::Json { value }]),
            ..ToolOutput::default()
        })
    }
}

#[async_trait]
impl Tool for ArtifactReadTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let token = CancellationToken::new();
        let _cancel_on_drop = CancelOnDrop(token.clone());
        tokio::select! {
            biased;
            _ = context.cancelled() => {
                token.cancel();
                Err(ToolError::Failed("artifact read cancelled".into()))
            }
            result = self.read_arguments(arguments, token.clone()) => result.map_err(|e| ToolError::Failed(e.to_string())),
        }
    }
}

//! In-process local media readers. No OCR, PDF rendering, remote URLs or shell
//! programs. File paths use the same confinement rules as local text tools.
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use eliteforge_agent_core::*;
use eliteforge_agent_official_plugin_tools_local::{
    ConfinedFile, ConfinedFileReader, FileReferenceReaderService, FileSystemToolsConfig,
};
#[cfg(test)]
use eliteforge_agent_test_support::TestBuilder;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::{Cursor, Write},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct MediaToolsConfig {
    reader: ConfinedFileReader,
    max_bytes: usize,
    max_pixels: u64,
    max_pages: u32,
    max_text_bytes: usize,
    image_tokens: Option<usize>,
}
impl MediaToolsConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            reader: ConfinedFileReader::new(root),
            max_bytes: 8 * 1024 * 1024,
            max_pixels: 16_000_000,
            max_pages: 10,
            max_text_bytes: 64 * 1024,
            image_tokens: None,
        }
    }
    pub fn with_filesystem_scope(mut self, scope: FileSystemToolsConfig) -> Self {
        self.reader = ConfinedFileReader::from_config(scope);
        self
    }
    pub fn with_max_bytes(mut self, bytes: usize) -> Self {
        self.max_bytes = bytes.clamp(1, 32 * 1024 * 1024);
        self
    }
    pub fn with_max_pixels(mut self, pixels: u64) -> Self {
        self.max_pixels = pixels.clamp(1, 32_000_000);
        self
    }
    pub fn with_pdf_limits(mut self, pages: u32, text_bytes: usize) -> Self {
        self.max_pages = pages.clamp(1, 100);
        self.max_text_bytes = text_bytes.clamp(1, 1024 * 1024);
        self
    }
    /// Supply the host/provider's media token estimate for budgeted model calls.
    /// None deliberately preserves the core's fail-closed budget behavior.
    pub fn with_image_token_estimate(mut self, tokens: usize) -> Self {
        self.image_tokens = Some(tokens.max(1));
        self
    }
}

#[derive(Clone)]
pub struct MediaToolsPlugin {
    config: MediaToolsConfig,
}
impl MediaToolsPlugin {
    pub fn new(config: MediaToolsConfig) -> Self {
        Self { config }
    }
}
#[async_trait]
impl Plugin for MediaToolsPlugin {
    fn id(&self) -> &str {
        "framework.media.tools"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        ToolProviderPlugin::new(
            self.id(),
            MediaProvider {
                config: self.config.clone(),
                slots: Arc::new(tokio::sync::Semaphore::new(2)),
            },
        )
        .install(context)
        .await
    }
}
/// Reusable provider for run-scoped host installation and unified discovery.
#[async_trait]
impl ToolProvider for MediaToolsPlugin {
    fn descriptors(&self) -> Option<Vec<ToolDescriptor>> {
        Some(media_tool_catalog())
    }
    async fn lease(&self, context: &RunContext) -> Result<ToolLease, AgentError> {
        MediaProvider {
            config: self.config.clone(),
            slots: Arc::new(tokio::sync::Semaphore::new(2)),
        }
        .lease(context)
        .await
    }
}

pub fn media_tool_catalog() -> Vec<ToolDescriptor> {
    [true, false].into_iter().map(|image| {
        let (id, name, description) = if image { ("media.read_image", "media_read_image", "Read a scoped local image or attachment as native media; validates size and dimensions.") }
            else { ("media.read_pdf", "media_read_pdf", "Read selected PDF pages from a scoped local file or attachment. Returns text and page numbers; no OCR.") };
        let mut schema = json!({"type":"object","additionalProperties":false,"oneOf":[{"required":["path"]},{"required":["fileRef"]}],"properties":{"path":{"type":"string","minLength":1},"fileRef":{"type":"object"}}});
        if !image { schema["properties"]["startPage"]=json!({"type":"integer","minimum":1}); schema["properties"]["endPage"]=json!({"type":"integer","minimum":1}); }
        ToolDescriptor::new(id, name, description, schema)
    }).collect()
}

struct MediaProvider {
    config: MediaToolsConfig,
    slots: Arc<tokio::sync::Semaphore>,
}
#[async_trait]
impl ToolProvider for MediaProvider {
    async fn lease(&self, _: &RunContext) -> Result<ToolLease, AgentError> {
        Ok(ToolLease::new(
            media_tool_catalog()
                .into_iter()
                .enumerate()
                .map(|(index, descriptor)| {
                    ToolBinding::new(
                        descriptor,
                        MediaTool {
                            config: self.config.clone(),
                            slots: self.slots.clone(),
                            image: index == 0,
                        },
                    )
                    .parallel_safe()
                })
                .collect(),
        ))
    }
}
struct MediaTool {
    config: MediaToolsConfig,
    slots: Arc<tokio::sync::Semaphore>,
    image: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Input {
    path: Option<String>,
    file_ref: Option<Value>,
    start_page: Option<u32>,
    end_page: Option<u32>,
}
#[async_trait]
impl Tool for MediaTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input: Input = serde_json::from_value(arguments).map_err(|e| failed(e.to_string()))?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let permit = tokio::select! {biased;
            _=context.cancellation.cancelled()=>return Err(failed("media read cancelled")),
            permit=tokio::time::timeout_at(deadline,self.slots.clone().acquire_owned())=>permit.map_err(|_|failed("media read timed out"))?.map_err(|_|failed("media reader closed"))?
        };
        let referenced = match (&input.path, &input.file_ref) {
            (Some(_), None) => None,
            (None, Some(reference)) => {
                let service = context
                    .extensions
                    .get::<FileReferenceReaderService>()
                    .ok_or_else(|| failed("file reference reader is not installed"))?;
                let file = tokio::select! {biased;
                    _ = context.cancellation.cancelled() => return Err(failed("media read cancelled")),
                    result = tokio::time::timeout_at(deadline, service.0.read(reference.clone(), self.config.max_bytes, context)) => result.map_err(|_| failed("media read timed out"))??,
                };
                if file.bytes.len() > self.config.max_bytes {
                    return Err(failed("media exceeds byte limit"));
                }
                Some(ConfinedFile {
                    path: file.name,
                    bytes: file.bytes,
                })
            }
            _ => return Err(failed("provide exactly one of path or fileRef")),
        };
        let config = self.config.clone();
        let mut worker_context = context.clone();
        worker_context.cancellation = context.cancellation.child_token();
        let _cancel_worker_on_exit = worker_context.cancellation.clone().drop_guard();
        let image = self.image;
        // The permit stays in the blocking worker even if the caller is dropped,
        // so cancellation cannot create unbounded parallel decoder workers.
        let worker = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            active(&worker_context)?;
            let file = match referenced {
                Some(file) => file,
                None => config.reader.read(
                    input.path.as_deref().expect("validated path"),
                    config.max_bytes,
                )?,
            };
            active(&worker_context)?;
            if image {
                read_image(&config, file.path, &file.bytes, &worker_context)
            } else {
                read_pdf(&config, file.path, &file.bytes, input, &worker_context)
            }
        });
        tokio::select! {biased;
            _=context.cancellation.cancelled()=>Err(failed("media read cancelled")),
            result=tokio::time::timeout_at(deadline,worker)=>result.map_err(|_|failed("media read timed out"))?.map_err(|_|failed("media decoder failed while parsing the file"))?
        }
    }
}

fn read_image(
    config: &MediaToolsConfig,
    path: String,
    bytes: &[u8],
    context: &RunContext,
) -> Result<ToolOutput, ToolError> {
    let format = image::guess_format(bytes).map_err(|_| failed("unrecognized image format"))?;
    let mime = match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::WebP => "image/webp",
        image::ImageFormat::Gif => "image/gif",
        _ => return Err(failed("only PNG/JPEG/WebP/GIF images are supported")),
    };
    let (width, height) = image::ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|e| failed(e.to_string()))?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > config.max_pixels {
        return Err(failed("image exceeds pixel limit"));
    }
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(width);
    limits.max_image_height = Some(height);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    reader
        .decode()
        .map_err(|e| failed(format!("invalid image: {e}")))?;
    active(context)?;
    let data =
        json!({"path":path,"mimeType":mime,"width":width,"height":height,"sizeBytes":bytes.len()});
    Ok(ToolOutput {
        structured: Some(data.clone()),
        content: Content(vec![
            ContentBlock::Text {
                text: data.to_string(),
            },
            ContentBlock::Media {
                media: MediaContent {
                    mime_type: mime.into(),
                    source: MediaSource::Base64(STANDARD.encode(bytes)),
                    name: Some(path),
                    estimated_tokens: config.image_tokens,
                },
            },
        ]),
        ..Default::default()
    })
}

struct TextBuffer<'a> {
    bytes: Vec<u8>,
    limit: usize,
    truncated: bool,
    context: &'a RunContext,
    started: Instant,
}
impl Write for TextBuffer<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.context.is_cancelled() || self.started.elapsed() > Duration::from_secs(10) {
            return Err(std::io::Error::other(
                "PDF extraction cancelled or timed out",
            ));
        }
        if self.bytes.len().saturating_add(bytes.len()) > self.limit {
            self.truncated = true;
            return Err(std::io::Error::other("PDF text byte limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn read_pdf(
    config: &MediaToolsConfig,
    path: String,
    bytes: &[u8],
    input: Input,
    context: &RunContext,
) -> Result<ToolOutput, ToolError> {
    if !bytes.starts_with(b"%PDF-") {
        return Err(failed("file is not a PDF"));
    }
    let start = input.start_page.unwrap_or(1);
    let requested_end = input
        .end_page
        .unwrap_or(start.saturating_add(config.max_pages - 1));
    if start == 0 || requested_end < start || requested_end - start >= config.max_pages {
        return Err(failed(
            "invalid page range or page count exceeds host limit",
        ));
    }
    let doc = pdf_extract::Document::load_mem_with_options(
        bytes,
        pdf_extract::LoadOptions {
            strict: true,
            ..Default::default()
        },
    )
    .map_err(|e| failed(format!("invalid PDF: {e}")))?;
    active(context)?;
    if doc.is_encrypted() {
        return Err(failed("encrypted PDF is not supported"));
    }
    let count = doc.get_pages().len();
    if count > 1000 || start as usize > count {
        return Err(failed(
            "PDF page count or requested start page is out of range",
        ));
    }
    let end = requested_end.min(count as u32);
    let started = Instant::now();
    let mut rows = Vec::new();
    let mut total = 0usize;
    let mut truncated = false;
    for page in start..=end {
        active(context)?;
        let mut buffer = TextBuffer {
            bytes: Vec::new(),
            limit: config.max_text_bytes.saturating_sub(total),
            truncated: false,
            context,
            started,
        };
        let result = pdf_extract::output_doc_page(
            &doc,
            &mut pdf_extract::PlainTextOutput::new(&mut buffer as &mut dyn Write),
            page,
        );
        active(context)?;
        if let Err(error) = result
            && !buffer.truncated
        {
            return Err(failed(format!(
                "PDF page {page} extraction failed: {error}"
            )));
        }
        let text = String::from_utf8(buffer.bytes)
            .map_err(|_| failed("PDF extraction produced invalid UTF-8"))?;
        total += text.len();
        truncated = buffer.truncated;
        rows.push(json!({"page":page,"text":text,"hasText":!text.trim().is_empty(),"truncated":buffer.truncated}));
        if truncated {
            break;
        }
    }
    let last = rows
        .last()
        .and_then(|r| r["page"].as_u64())
        .unwrap_or(start as u64) as u32;
    // A truncated page cannot be resumed at its missing byte offset. Say so
    // explicitly instead of offering a cursor which would silently skip text.
    let data = json!({"path":path,"totalPages":count,"startPage":start,"endPage":last,"pages":rows,"truncated":truncated,"nextPage":if !truncated && last<count as u32 {Some(last+1)} else {None},"ocr":false});
    let mut output = ToolOutput::text(data.to_string());
    output.structured = Some(data);
    Ok(output)
}

fn active(context: &RunContext) -> Result<(), ToolError> {
    if context.is_cancelled() {
        Err(failed("media read cancelled"))
    } else {
        Ok(())
    }
}
fn failed(message: impl Into<String>) -> ToolError {
    ToolError::Failed(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    use eliteforge_agent_test_support::TestConversationStore;

    struct AttachmentReader;
    #[async_trait]
    impl eliteforge_agent_official_plugin_tools_local::FileReferenceReader for AttachmentReader {
        async fn read(
            &self,
            reference: Value,
            _: usize,
            _: &RunContext,
        ) -> Result<eliteforge_agent_official_plugin_tools_local::ReferencedFile, ToolError>
        {
            if reference != json!({"id":"allowed"}) {
                return Err(failed("attachment access denied"));
            }
            let mut bytes = Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(1, 1)
                .write_to(&mut bytes, image::ImageFormat::Png)
                .unwrap();
            Ok(
                eliteforge_agent_official_plugin_tools_local::ReferencedFile {
                    name: "attachment.png".into(),
                    bytes: bytes.into_inner(),
                    reference,
                },
            )
        }
    }
    struct AttachmentRead;
    #[async_trait]
    impl AgentLoop for AttachmentRead {
        async fn run(
            &self,
            _: &RunRequest,
            context: &RunContext,
            services: &AgentServices,
        ) -> Result<RunResult, AgentError> {
            let lease = services.tools[0].lease(context).await?;
            let tool = &lease.tools[0].executor;
            let output = tool
                .call(json!({"fileRef":{"id":"allowed"}}), context)
                .await
                .unwrap();
            assert_eq!(output.structured.as_ref().unwrap()["width"], 1);
            assert!(
                output
                    .content
                    .0
                    .iter()
                    .any(|block| matches!(block, ContentBlock::Media { .. }))
            );
            let denied = tool
                .call(json!({"fileRef":{"id":"other-session"}}), context)
                .await
                .unwrap_err();
            assert!(denied.to_string().contains("access denied"));
            assert!(
                tool.call(json!({"path":"a.png","fileRef":{"id":"allowed"}}), context)
                    .await
                    .is_err()
            );
            assert!(
                tool.call(json!({"path":"../outside.png"}), context)
                    .await
                    .is_err()
            );
            Ok(RunResult {
                output: String::new(),
                content: Content::default(),
                structured: None,
                status: RunStatus::Completed,
                interactions: vec![],
                usage: Usage::default(),
                steps: 0,
                tool_calls: 0,
                external_session: None,
            })
        }
    }
    #[tokio::test]
    async fn media_attachments_use_host_authority_and_preserve_native_content() {
        let agent = TestBuilder::new()
            .default_models(false)
            .plugin(ConversationStorePlugin::new(
                TestConversationStore::default(),
            ))
            .plugin(AgentLoopPlugin::new(AttachmentRead))
            .plugin(MediaToolsPlugin::new(MediaToolsConfig::new(".")))
            .build()
            .await
            .unwrap();
        let mut request = RunRequest::new("attachment", "read image");
        request
            .extensions
            .insert(FileReferenceReaderService(Arc::new(AttachmentReader)));
        let result = agent.run(request).await.unwrap();
        assert_eq!(result.status, RunStatus::Completed);
        assert!(agent.framework().close().await.is_success());
    }

    struct QueueTimeout;
    #[async_trait]
    impl AgentLoop for QueueTimeout {
        async fn run(
            &self,
            _: &RunRequest,
            context: &RunContext,
            services: &AgentServices,
        ) -> Result<RunResult, AgentError> {
            let lease = services.tools[0].lease(context).await?;
            let error = lease.tools[0]
                .executor
                .call(json!({"path":"unused.png"}), context)
                .await
                .expect_err("occupied decoder slots must time out");
            assert!(error.to_string().contains("timed out"));
            assert!(
                !context.is_cancelled(),
                "tool timeout must not cancel the parent run"
            );
            Ok(RunResult {
                output: String::new(),
                content: Content::default(),
                structured: None,
                status: RunStatus::Completed,
                interactions: vec![],
                usage: Usage::default(),
                steps: 0,
                tool_calls: 0,
                external_session: None,
            })
        }
    }

    #[tokio::test]
    async fn waiting_for_a_busy_decoder_is_bounded_without_cancelling_the_run() {
        let agent = TestBuilder::new()
            .default_models(false)
            .plugin(ConversationStorePlugin::new(
                TestConversationStore::default(),
            ))
            .plugin(AgentLoopPlugin::new(QueueTimeout))
            .plugin(ToolProviderPlugin::new(
                "test.media",
                MediaProvider {
                    config: MediaToolsConfig::new("."),
                    slots: Arc::new(tokio::sync::Semaphore::new(0)),
                },
            ))
            .build()
            .await
            .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            agent.run(RunRequest::new("timeout", "read image")),
        )
        .await
        .expect("must return within the deadline")
        .unwrap();
        assert_eq!(result.status, RunStatus::Completed);
        assert!(agent.framework().close().await.is_success());
    }
}

use eliteforge_agent_context::*;
use eliteforge_agent_core::*;
fn block(cost: Option<usize>) -> ContextBlock {
    ContextBlock::data(
        "evidence",
        Content::from(vec![
            ContentBlock::Text {
                text: "source paragraph: chart rises from 1 to 4".into(),
            },
            ContentBlock::Media {
                media: MediaContent {
                    mime_type: "image/png".into(),
                    source: MediaSource::Base64("synthetic".into()),
                    name: Some("private-figure-1".into()),
                    estimated_tokens: cost,
                },
            },
        ]),
    )
}
#[tokio::test]
async fn native_media_admission_preserves_text_and_reports_budget_omissions() {
    let model = ModelRef::new("mock", "vision");
    let estimator = ConservativeTokenEstimator;
    let included = project_multimodal_data(
        &block(Some(2048)),
        ContextBudget {
            max_tokens: 5000,
            max_messages: 1,
        },
        &model,
        &estimator,
        4,
        1200,
        true,
    )
    .await
    .unwrap();
    assert!(included.messages[0].content.has_media());
    assert!(included.report.final_tokens <= 5000);
    assert_eq!(
        included.report.media[0].outcome,
        MediaAdmissionOutcome::Included
    );
    let omitted = project_multimodal_data(
        &block(Some(2048)),
        ContextBudget {
            max_tokens: 1200,
            max_messages: 1,
        },
        &model,
        &estimator,
        4,
        1200,
        true,
    )
    .await
    .unwrap();
    assert!(!omitted.messages[0].content.has_media());
    assert!(
        omitted.messages[0]
            .content
            .text()
            .contains("source paragraph")
    );
    assert!(
        omitted.messages[0]
            .content
            .text()
            .contains("not shown to the model")
    );
    assert_eq!(
        omitted.report.media[0].outcome,
        MediaAdmissionOutcome::Budget
    );
}
#[tokio::test]
async fn missing_cost_text_model_and_zero_quota_fail_closed() {
    let model = ModelRef::new("mock", "vision");
    let estimator = ConservativeTokenEstimator;
    let budget = ContextBudget {
        max_tokens: 5000,
        max_messages: 1,
    };
    for (cost, quota, vision, expected) in [
        (None, 4, true, MediaAdmissionOutcome::UnknownCost),
        (Some(20), 0, true, MediaAdmissionOutcome::Quota),
        (Some(20), 4, false, MediaAdmissionOutcome::UnsupportedModel),
    ] {
        let result = project_multimodal_data(
            &block(cost),
            budget,
            &model,
            &estimator,
            quota,
            1200,
            vision,
        )
        .await
        .unwrap();
        assert!(!result.messages[0].content.has_media());
        assert_eq!(result.report.media[0].outcome, expected);
    }
    let empty = project_multimodal_data(
        &block(Some(20)),
        ContextBudget {
            max_tokens: 0,
            max_messages: 0,
        },
        &model,
        &estimator,
        4,
        1200,
        true,
    )
    .await
    .unwrap();
    assert!(empty.messages.is_empty());
}
#[tokio::test]
async fn provider_estimator_and_media_order_are_authoritative() {
    struct Expensive;
    #[async_trait::async_trait]
    impl TokenEstimator for Expensive {
        async fn message(&self, _: &ModelRef, m: &Message) -> Result<usize, ModelError> {
            Ok(m.content.text().len() + 16 + if m.content.has_media() { 9000 } else { 0 })
        }
    }
    let model = ModelRef::new("mock", "vision");
    let budget = ContextBudget {
        max_tokens: 5000,
        max_messages: 1,
    };
    let result =
        project_multimodal_data(&block(Some(20)), budget, &model, &Expensive, 4, 1200, true)
            .await
            .unwrap();
    assert_eq!(
        result.report.media[0].outcome,
        MediaAdmissionOutcome::Budget
    );
    let mut source = block(Some(100));
    let mut extra = source.messages[0].content.0[1].clone();
    if let ContentBlock::Media { media } = &mut extra {
        media.name = Some("figure-2".into());
    }
    source.messages[0].content.0.push(extra);
    let result = project_multimodal_data(
        &source,
        budget,
        &model,
        &ConservativeTokenEstimator,
        1,
        1200,
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        result.report.media[0].outcome,
        MediaAdmissionOutcome::Included
    );
    assert_eq!(result.report.media[1].outcome, MediaAdmissionOutcome::Quota);
    source.trust = ContextTrust::Instruction;
    assert!(
        project_multimodal_data(
            &source,
            budget,
            &model,
            &ConservativeTokenEstimator,
            4,
            1200,
            true
        )
        .await
        .is_err()
    );
}

#[derive(Default)]
struct Capture(std::sync::Mutex<Vec<ChatRequest>>);
#[async_trait::async_trait]
impl ChatModel for Capture {
    async fn generate(
        &self,
        _: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        self.0.lock().unwrap().push(request);
        Ok(ChatResponse::text("mock-only visual answer"))
    }
}
struct NativeMediaWorkflow {
    budget: usize,
    cancel: bool,
    audit: MediaContextAudit,
}
#[async_trait::async_trait]
impl eliteforge_agent_framework::ModelWorkflow for NativeMediaWorkflow {
    async fn run(
        &self,
        run: &RunContext,
        models: &dyn eliteforge_agent_framework::ChatExecutor,
        limits: &RunLimits,
        model: &ModelRef,
    ) -> Result<ChatResponse, AgentError> {
        let mut run = run.clone();
        run.extensions.insert(self.audit.clone());
        if self.cancel {
            run.cancellation.cancel();
        }
        let source = block(Some(2048));
        let history =
            ConversationSnapshot::new(ConversationKey::new("synthetic-media"), "fixture", vec![]);
        let messages = BoundMultimodalData::default()
            .reduce(&ContextReductionInput {
                block: &source,
                budget: ContextBudget {
                    max_tokens: self.budget,
                    max_messages: 4,
                },
                model,
                estimator: &ConservativeTokenEstimator,
                history: &history,
                run: &run,
                models,
                limits,
                overflow_recoveries: 0,
                compression: None,
            })
            .await?;
        models
            .invoke_auxiliary(
                model,
                ChatRequest {
                    messages,
                    tools: vec![],
                    options: GenerationOptions::default(),
                    extensions: run.extensions.clone(),
                },
                "synthetic-visual-answer",
                limits,
                &run,
            )
            .await
    }
}
#[tokio::test]
async fn sdk_reducer_sends_native_media_with_citation_and_cancels_before_dispatch() {
    use eliteforge_agent_framework::{ModelClient, ModelRuntimeUpdate, RunLimits};
    use std::sync::Arc;
    for (budget, cancel, expected_media) in [
        (5000, false, true),
        (1200, false, false),
        (5000, true, false),
    ] {
        let provider = Arc::new(Capture::default());
        let audit = MediaContextAudit::default();
        let descriptor =
            ModelDescriptor::new("vision").with_input_modalities([Modality::Text, Modality::Image]);
        let client = ModelClient::new(ModelRuntimeUpdate::new(
            "synthetic",
            1,
            vec![ModelConnection::new(
                "mock",
                provider.clone(),
                vec![descriptor],
            )],
        ))
        .unwrap();
        let limits = RunLimits {
            max_model_retries: 0,
            max_context_tokens: 8000,
            max_model_output_tokens: 128,
            max_total_tokens: 20000,
            ..Default::default()
        };
        let result = client
            .workflow_observed(
                &ModelRef::new("mock", "vision"),
                Arc::new(NativeMediaWorkflow {
                    budget,
                    cancel,
                    audit: audit.clone(),
                }),
                &limits,
                "native-media-test",
            )
            .await
            .into_result();
        if cancel {
            assert!(result.is_err());
            assert!(provider.0.lock().unwrap().is_empty());
        } else {
            result.unwrap();
            let requests = provider.0.lock().unwrap();
            assert_eq!(requests.len(), 1);
            let message = &requests[0].messages[0];
            assert_eq!(message.content.has_media(), expected_media);
            assert!(message.content.text().contains("source paragraph"));
            if expected_media {
                let blocks = &message.content.0;
                let index = blocks
                    .iter()
                    .position(|b| matches!(b, ContentBlock::Media { .. }))
                    .unwrap();
                assert!(
                    matches!(&blocks[index-1],ContentBlock::Text{text} if text.contains("private-figure-1"))
                );
            }
            assert_eq!(audit.0.lock().unwrap().len(), 1);
        }
        client.close().await;
    }
}

#[tokio::test]
async fn visual_budget_borrows_history_without_displacing_instructions_or_latest_turn() {
    use std::sync::Arc;
    struct Base;
    #[async_trait::async_trait]
    impl ContextBudgetPolicy for Base {
        async fn allocate(
            &self,
            _: &[ContextBlockConfig],
            _: ContextBudget,
        ) -> Result<Vec<ContextBudget>, AgentError> {
            unreachable!()
        }
        async fn allocate_sources(
            &self,
            _: &[ContextBlockConfig],
            _: ContextBudget,
            _: &[usize],
        ) -> Result<Vec<ContextBudget>, AgentError> {
            Ok([1000, 1000, 8000]
                .into_iter()
                .map(|max_tokens| ContextBudget {
                    max_tokens,
                    max_messages: 16,
                })
                .collect())
        }
    }
    let model = ModelRef::new("mock", "vision");
    let mut tool = Message::new(Role::Tool, "result");
    tool.tool_call_id = Some("call".into());
    let messages = vec![
        Message::user("old turn"),
        Message::user("current question"),
        tool,
    ];
    let protected = required_recent_turn_tokens(&messages, &model, &ConservativeTokenEstimator)
        .await
        .unwrap();
    assert!(protected > 32);
    let blocks = vec![
        ContextBlockConfig::new(INSTRUCTIONS_BLOCK, BlockBudget::tokens(1000)),
        ContextBlockConfig::new("visual", BlockBudget::tokens(7000)),
        ContextBlockConfig::new(HISTORY_BLOCK, BlockBudget::tokens(10000)),
    ];
    let policy = BorrowHistoryForData {
        base: Arc::new(Base),
        block_id: "visual".into(),
        max_tokens: 7000,
        protected_history_tokens: protected,
    };
    let budgets = policy
        .allocate_sources(
            &blocks,
            ContextBudget {
                max_tokens: 10000,
                max_messages: 16,
            },
            &[1000, 9000, 500],
        )
        .await
        .unwrap();
    assert_eq!(budgets[0].max_tokens, 1000);
    assert_eq!(budgets[1].max_tokens, 7000);
    assert_eq!(budgets.iter().map(|b| b.max_tokens).sum::<usize>(), 10000);
    assert!(budgets[2].max_tokens >= protected);
    let constrained = BorrowHistoryForData {
        protected_history_tokens: 7500,
        ..policy
    };
    let budgets = constrained
        .allocate_sources(
            &blocks,
            ContextBudget {
                max_tokens: 10000,
                max_messages: 16,
            },
            &[1000, 9000, 8000],
        )
        .await
        .unwrap();
    assert_eq!(budgets[1].max_tokens, 1500);
    assert_eq!(budgets[2].max_tokens, 7500);
}

#[test]
fn image_preparation_is_bounded_preserves_identity_and_composites_transparency() {
    use base64::Engine;
    use std::io::Cursor;
    let original = image::RgbaImage::from_pixel(1024, 256, image::Rgba([0, 0, 0, 0]));
    let mut png = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(original)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    let media = prepare_inline_image_evidence(
        png.get_ref(),
        Some("private resource: source-7 block-2".into()),
        ImageEvidenceOptions::default(),
    )
    .unwrap();
    assert!(media.name.as_ref().unwrap().contains("source-7 block-2"));
    let MediaSource::Base64(data) = media.source else {
        panic!("resolved inline media required")
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .unwrap();
    assert!(bytes.len() <= 8192);
    let image = image::load_from_memory(&bytes).unwrap().to_rgb8();
    assert!(image.width() <= 768);
    assert_eq!(image.width(), image.height() * 4);
    assert!(image.get_pixel(0, 0)[0] > 250);
    assert!(
        prepare_inline_image_evidence(b"not an image", None, ImageEvidenceOptions::default())
            .is_err()
    );
    assert!(
        prepare_inline_image_evidence(
            png.get_ref(),
            None,
            ImageEvidenceOptions {
                max_side: 1,
                max_encoded_bytes: 4096
            }
        )
        .is_err()
    );
}

#[cfg(target_os = "macos")]
#[test]
fn local_pdf_rasterization_preserves_orientation_and_bounds() {
    use eliteforge_agent_context::{PdfEvidenceOptions, render_pdf_evidence};
    // Synthetic two color patches, no user data or network requests.
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R >>",
        "<< /Length 51 >>\nstream\n1 0 0 rg 0 60 50 40 re f\n0 0 1 rg 150 0 50 40 re f\nendstream",
    ];
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = vec![0];
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", i + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
    for offset in &offsets[1..] {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    let rendered = render_pdf_evidence(
        &pdf,
        PdfEvidenceOptions {
            max_pages: 1,
            max_side: 200,
        },
    )
    .unwrap();
    assert_eq!(rendered.len(), 1);
    assert_eq!((rendered[0].width, rendered[0].height), (200, 100));
    let image = image::load_from_memory(&rendered[0].png).unwrap().to_rgb8();
    assert_eq!(
        image.get_pixel(10, 10).0,
        [255, 0, 0],
        "corners {:?} {:?} {:?}",
        image.get_pixel(10, 90),
        image.get_pixel(190, 10),
        image.get_pixel(190, 90)
    );
    assert_eq!(image.get_pixel(190, 90).0, [0, 0, 255]);
    assert!(render_pdf_evidence(b"bad pdf", Default::default()).is_err());
    assert!(
        render_pdf_evidence(
            &pdf,
            PdfEvidenceOptions {
                max_pages: 0,
                max_side: 200
            }
        )
        .is_err()
    );
}

#[tokio::test]
async fn structured_data_shares_the_configured_text_budget() {
    let source = ContextBlock::data(
        "runtime",
        Content::from(vec![ContentBlock::Json {
            value: serde_json::json!({"kind":"application_context", "text":"scope-A", "payload":"x".repeat(2000)}),
        }]),
    );
    let model = ModelRef::new("mock", "text");
    for cap in [128, 4096] {
        let result = project_multimodal_data(
            &source,
            ContextBudget {
                max_tokens: cap,
                max_messages: 1,
            },
            &model,
            &ConservativeTokenEstimator,
            0,
            cap,
            false,
        )
        .await
        .unwrap();
        assert!(result.report.final_tokens <= cap);
        assert!(!result.messages.is_empty());
        assert!(result.messages.iter().all(|m| m.role == Role::User));
        if cap == 4096 {
            assert!(result.messages[0].content.text().contains("scope-A"));
        }
    }
}

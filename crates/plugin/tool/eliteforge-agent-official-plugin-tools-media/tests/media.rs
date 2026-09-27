mod support;
use base64::{Engine, engine::general_purpose::STANDARD};
use eliteforge_agent_core::{ContentBlock, MediaSource};
use eliteforge_agent_official_plugin_tools_media::*;
use serde_json::json;
use std::path::Path;
use support::invoke;

fn pdf(path: &Path) {
    use pdf_extract::{
        Document, Object, Stream,
        content::{Content, Operation},
        dictionary,
    };
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font =
        doc.add_object(dictionary! {"Type"=>"Font","Subtype"=>"Type1","BaseFont"=>"Helvetica"});
    let resources = doc.add_object(dictionary! {"Font"=>dictionary!{"F1"=>font}});
    let mut kids = Vec::new();
    for text in ["First page", "Second page", ""] {
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![10.into(), 100.into()]),
                Operation::new("Tj", vec![Object::string_literal(text)]),
                Operation::new("ET", vec![]),
            ],
        };
        let stream = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page=doc.add_object(dictionary!{"Type"=>"Page","Parent"=>pages_id,"Contents"=>stream,"Resources"=>resources,"MediaBox"=>vec![0.into(),0.into(),300.into(),300.into()]});
        kids.push(page.into());
    }
    doc.objects.insert(
        pages_id,
        dictionary! {"Type"=>"Pages","Kids"=>kids,"Count"=>3}.into(),
    );
    let catalog = doc.add_object(dictionary! {"Type"=>"Catalog","Pages"=>pages_id});
    doc.trailer.set("Root", catalog);
    doc.save(path).unwrap();
}

#[tokio::test]
async fn image_returns_validated_native_media_and_respects_pixel_and_path_limits() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("test.png");
    image::RgbImage::new(2, 3).save(&path).unwrap();
    let plugin =
        MediaToolsPlugin::new(MediaToolsConfig::new(root.path()).with_image_token_estimate(1000));
    let output = invoke(
        plugin,
        "media_read_image",
        json!({"path":"test.png"}),
        false,
    )
    .await
    .unwrap();
    assert_eq!(output.structured.as_ref().unwrap()["height"], 3);
    let media = output
        .content
        .0
        .iter()
        .find_map(|b| {
            if let ContentBlock::Media { media } = b {
                Some(media)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(media.mime_type, "image/png");
    assert_eq!(media.estimated_tokens, Some(1000));
    let MediaSource::Base64(encoded) = &media.source else {
        panic!("inline image expected")
    };
    assert_eq!(
        STANDARD.decode(encoded).unwrap(),
        std::fs::read(path).unwrap()
    );
    for (config, file) in [
        (
            MediaToolsConfig::new(root.path()).with_max_pixels(1),
            "test.png",
        ),
        (MediaToolsConfig::new(root.path()), "../escape"),
        (
            MediaToolsConfig::new(root.path()).with_max_bytes(4),
            "test.png",
        ),
    ] {
        assert!(
            invoke(
                MediaToolsPlugin::new(config),
                "media_read_image",
                json!({"path":file}),
                false
            )
            .await
            .is_err()
        );
    }
    assert!(
        invoke(
            MediaToolsPlugin::new(MediaToolsConfig::new(root.path())),
            "media_read_image",
            json!({"path":"test.png"}),
            true
        )
        .await
        .unwrap_err()
        .contains("cancelled")
    );
}

#[tokio::test]
async fn pdf_extracts_selected_pages_reports_empty_pages_and_bounds_output() {
    let root = tempfile::tempdir().unwrap();
    pdf(&root.path().join("test.pdf"));
    let plugin = || MediaToolsPlugin::new(MediaToolsConfig::new(root.path()));
    let output = invoke(
        plugin(),
        "media_read_pdf",
        json!({"path":"test.pdf","startPage":2,"endPage":2}),
        false,
    )
    .await
    .unwrap()
    .structured
    .unwrap();
    assert_eq!(output["totalPages"], 3);
    assert_eq!(output["pages"][0]["page"], 2);
    assert_eq!(output["nextPage"], 3);
    assert!(
        output["pages"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Second page")
    );
    let empty = invoke(
        plugin(),
        "media_read_pdf",
        json!({"path":"test.pdf","startPage":3}),
        false,
    )
    .await
    .unwrap()
    .structured
    .unwrap();
    assert_eq!(empty["pages"][0]["hasText"], false);
    assert_eq!(empty["ocr"], false);
    let bounded = MediaToolsPlugin::new(MediaToolsConfig::new(root.path()).with_pdf_limits(1, 5));
    let result = invoke(bounded, "media_read_pdf", json!({"path":"test.pdf"}), false)
        .await
        .unwrap()
        .structured
        .unwrap();
    assert_eq!(result["truncated"], true);
    assert!(result["pages"][0]["text"].as_str().unwrap().len() <= 5);
    assert!(result["nextPage"].is_null());
    std::fs::write(root.path().join("bad.pdf"), b"%PDF-1.5\ninvalid").unwrap();
    assert!(
        invoke(plugin(), "media_read_pdf", json!({"path":"bad.pdf"}), false)
            .await
            .is_err()
    );
    assert!(
        invoke(
            plugin(),
            "media_read_pdf",
            json!({"path":"test.pdf","startPage":4}),
            false
        )
        .await
        .is_err()
    );
}

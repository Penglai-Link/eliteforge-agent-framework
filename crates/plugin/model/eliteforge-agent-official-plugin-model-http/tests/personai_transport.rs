use crab_serverapi::{AuthorizedClientContext, gateway::QualityType};
use eliteforge_agent_official_plugin_model_http::*;
use rsa::pkcs8::EncodePublicKey;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
async fn check_personai_error(
    status: u16,
    body: &str,
    category: eliteforge_agent_core::ProviderErrorCategory,
) {
    let body = body.to_owned();
    let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_url = format!("http://{}/redirected", target.local_addr().unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        let (end, len) = loop {
            let n = socket.read(&mut buffer).await.unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&buffer[..n]);
            if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..end]);
                let len = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|n| n.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                break (end + 4, len);
            }
        };
        while bytes.len() < end + len {
            let n = socket.read(&mut buffer).await.unwrap();
            bytes.extend_from_slice(&buffer[..n]);
        }
        let headers = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
        assert!(headers.contains("x-product-code: tron"));
        assert!(headers.contains("authorization: bearer credential"));
        assert!(
            headers
                .lines()
                .next()
                .unwrap()
                .contains("/chat/completions ")
        );
        let encrypted: serde_json::Value = serde_json::from_slice(&bytes[end..end + len]).unwrap();
        assert!(encrypted.get("messages").is_none(), "plaintext was sent");
        assert!(encrypted.as_object().is_some_and(|v| !v.is_empty()));
        let reply = format!(
            "HTTP/1.1 {status} Error\r\nContent-Length: {}\r\nLocation: {target_url}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(reply.as_bytes()).await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    });
    let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048)
        .unwrap()
        .to_public_key()
        .to_public_key_pem(rsa::pkcs8::LineEnding::LF)
        .unwrap();
    let context = AuthorizedClientContext::new()
        .with_token("credential")
        .with_client_id("fixture")
        .with_public_key(key)
        .with_product_code("Tron")
        .with_product_version("fixture");
    let transport =
        PersonAiModelTransport::new(url, context, QualityType::High, Some("conversation".into()))
            .unwrap();
    let error = transport
        .json(
            "/chat/completions",
            &json!({"messages":[{"role":"user","content":"hello"}]}),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error,eliteforge_agent_core::ModelError::Provider(error) if error.category==category)
    );
    server.await.unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), target.accept())
            .await
            .is_err(),
        "model redirect target must receive zero requests"
    );
}

#[tokio::test]
async fn personai_encrypted_model_transport_preserves_product_header_and_provider_error_categories()
{
    use eliteforge_agent_core::ProviderErrorCategory;
    check_personai_error(401, "设备已解绑", ProviderErrorCategory::AccountChanged).await;
    check_personai_error(
        429,
        r#"{"code":429,"message":"额度不足"}"#,
        ProviderErrorCategory::Quota,
    )
    .await;
    check_personai_error(
        404,
        r#"{"message":"no available model"}"#,
        ProviderErrorCategory::Unavailable,
    )
    .await;
    check_personai_error(307, "redirect", ProviderErrorCategory::Other).await;
    check_personai_error(308, "redirect", ProviderErrorCategory::Other).await;
}

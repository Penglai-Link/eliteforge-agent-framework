//! HTTP 语音识别协议适配；宿主只提供模型与连接配置。
use crate::{AgentModelConfig, AgentModelProtocol};
use async_trait::async_trait;
use base64::Engine;
use eliteforge_agent_core as fw;
use serde_json::{Value, json};

/// OpenAI 文件转写和 Gemini 音频输入；不把实时 WebSocket 当作文件转写。
pub struct HttpSpeechRecognitionModel {
    pub config: AgentModelConfig,
}

#[async_trait]
impl fw::SpeechRecognitionModel for HttpSpeechRecognitionModel {
    async fn transcribe(
        &self,
        _: &str,
        request: fw::SpeechRecognitionRequest,
    ) -> Result<fw::SpeechRecognitionResponse, fw::ModelError> {
        let model = request
            .choice
            .model
            .ok_or_else(|| fw::ModelError::Fatal("speech binding missing".into()))?;
        let fw::MediaSource::Base64(encoded) = request.audio.source else {
            return Err(fw::ModelError::Fatal("speech requires inline audio".into()));
        };
        let audio = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| fw::ModelError::Fatal(format!("invalid audio: {e}")))?;
        let text = transcribe(
            &self.config,
            audio,
            &request.audio.mime_type,
            request.audio.name.as_deref().unwrap_or("audio.wav"),
            request.language.as_deref(),
        )
        .await
        .map_err(fw::ModelError::Fatal)?;
        Ok(fw::SpeechRecognitionResponse {
            model,
            text,
            segments: Vec::new(),
        })
    }
}

fn redact(detail: &str, config: &AgentModelConfig) -> String {
    let mut value = detail.to_owned();
    for secret in std::iter::once(&config.api_key).chain(config.request_headers.values()) {
        if !secret.is_empty() {
            value = value.replace(secret, "[REDACTED]");
        }
    }
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(1000)
        .collect()
}

async fn transcribe(
    config: &AgentModelConfig,
    audio: Vec<u8>,
    mime: &str,
    file_name: &str,
    language: Option<&str>,
) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "could not create transcription client")?;
    let base = config.base_url.trim_end_matches('/');
    let mut request = match config.protocol {
        AgentModelProtocol::ChatCompletions | AgentModelProtocol::Responses => {
            let part = reqwest::multipart::Part::bytes(audio)
                .file_name(file_name.to_owned())
                .mime_str(mime)
                .map_err(|_| "invalid audio type")?;
            let mut form = reqwest::multipart::Form::new()
                .text("model", config.model_name.clone())
                .text("response_format", "json")
                .part("file", part);
            if let Some(language) = language.filter(|value| !value.is_empty()) {
                form = form.text("language", language.to_owned());
            }
            client
                .post(format!(
                    "{base}{}",
                    config
                        .api_path
                        .as_deref()
                        .unwrap_or("/audio/transcriptions")
                ))
                .bearer_auth(&config.api_key)
                .multipart(form)
        }
        AgentModelProtocol::Gemini => {
            let model = config.model_name.trim_start_matches("models/");
            let base = if base.ends_with("/v1beta") || base.ends_with("/v1") {
                base.to_owned()
            } else {
                format!("{base}/v1beta")
            };
            let mut url = reqwest::Url::parse(&format!("{base}/models/"))
                .map_err(|_| "invalid provider URL")?;
            url.path_segments_mut()
                .map_err(|_| "invalid provider URL")?
                .pop_if_empty()
                .push(&format!("{model}:generateContent"));
            client.post(url).header("x-goog-api-key", &config.api_key).json(&json!({
                "contents": [{"parts": [
                    {"text": "Transcribe the speech verbatim in its original language. Return only the transcript. If there is no speech, return an empty string."},
                    {"inlineData": {"mimeType": mime, "data": base64::engine::general_purpose::STANDARD.encode(audio)}}
                ]}]
            }))
        }
        _ => return Err("unsupported transcription protocol".into()),
    };
    for (name, value) in &config.request_headers {
        request = request.header(name, value);
    }
    let response = request.send().await.map_err(|e| {
        format!(
            "transcription transport failed: {}",
            redact(&e.to_string(), config)
        )
    })?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "transcription upstream HTTP {status}: {}",
            redact(&body, config)
        ));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|_| "invalid transcription response")?;
    if config.protocol == AgentModelProtocol::Gemini {
        let parts = body
            .pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
            .ok_or("missing transcription")?;
        Ok(parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""))
    } else {
        body.get("text")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "missing transcription".into())
    }
}

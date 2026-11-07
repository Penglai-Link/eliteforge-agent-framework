//! Full-duplex adapter for PersonAI's encrypted audio transcription WebSocket.
use std::sync::Arc;

use async_trait::async_trait;
use crab_serverapi::audio::AudioTranscriptionsClient;
use eliteforge_agent_core::{
    Content, MediaSource, Modality, ModelError, RealtimeEvent, RealtimeInput, RealtimeModel,
    RealtimeRequest, RealtimeSession,
};
#[cfg(test)]
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
#[cfg(test)]
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::models::BoundPersonAiModels;

pub struct PersonAiRealtimeAdapter {
    pub binding: Arc<BoundPersonAiModels>,
    pub server_base_url: String,
}

pub struct PersonAiRealtimeSession {
    commands: mpsc::Sender<Value>,
    events: Mutex<mpsc::Receiver<RealtimeEvent>>,
    cancel: CancellationToken,
}

impl Drop for PersonAiRealtimeSession {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[async_trait]
impl RealtimeModel for PersonAiRealtimeAdapter {
    async fn connect(
        &self,
        model: &str,
        request: RealtimeRequest,
        cancellation: CancellationToken,
    ) -> Result<Arc<dyn RealtimeSession>, ModelError> {
        self.binding.ensure_current()?;
        if model != "speech-auto"
            || request.input != vec![Modality::Audio]
            || request.output != vec![Modality::Text]
        {
            return Err(ModelError::Fatal(
                "PersonAI realtime supports speech-auto Audio → Text only".into(),
            ));
        }
        let account = self.binding.authorization();
        account
            .validate_gateway_access()
            .map_err(|error| ModelError::Fatal(error.to_string()))?;
        let client = AudioTranscriptionsClient::new(&self.server_base_url)
            .map_err(|e| ModelError::Fatal(e.to_string()))?;
        let mut websocket = client
            .connect(&account)
            .await
            .map_err(|e| ModelError::Transient(e.to_string()))?;
        websocket
            .start_with_sample_rate("", None, None, true, None, 24000)
            .await
            .map_err(|e| ModelError::Transient(e.to_string()))?;
        let (mut writer, mut reader) = websocket.split();
        let (command_tx, mut command_rx) = mpsc::channel::<Value>(32);
        let (event_tx, event_rx) = mpsc::channel::<RealtimeEvent>(64);
        let close = cancellation.child_token();
        let worker_cancel = close.clone();
        tokio::spawn(async move {
            // 外层取消可中断任何发送或事件背压，不能只在空闲的 select 中检查。
            let exchange = async {
                loop {
                    tokio::select! {
                        command = command_rx.recv() => match command {
                            Some(command) => writer.send_encrypted(&command).await?,
                            None => break,
                        },
                        incoming = reader.receive() => {
                            let value = incoming?;
                            if value["type"] == "stopped" || value["type"] == "session.finished" {
                                break;
                            }
                            if let Some(event) = map_event(&value) {
                                let failed = matches!(event, RealtimeEvent::Error(_));
                                if event_tx.send(event).await.is_err() || failed { break; }
                            }
                        }
                    }
                }
                Ok::<_, crab_serverapi::error::ServerApiError>(())
            };
            tokio::select! {
                biased;
                _ = worker_cancel.cancelled() => {},
                result = exchange => if let Err(error) = result {
                    let _ = event_tx.try_send(RealtimeEvent::Error(error.to_string()));
                }
            }
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), writer.close()).await;
            let _ = event_tx.try_send(RealtimeEvent::Completed);
        });
        Ok(Arc::new(PersonAiRealtimeSession {
            commands: command_tx,
            events: Mutex::new(event_rx),
            cancel: close,
        }))
    }
}

#[async_trait]
impl RealtimeSession for PersonAiRealtimeSession {
    async fn send(&self, input: RealtimeInput) -> Result<(), ModelError> {
        let payload = match input {
            RealtimeInput::Commit => json!({"type": "commit"}),
            RealtimeInput::Interrupt => json!({"type": "stop"}),
            RealtimeInput::Content(content) => audio_command(content)?,
        };
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(ModelError::Transient("PersonAI realtime session closed".into())),
            result = self.commands.send(payload) => result.map_err(|_| ModelError::Transient("PersonAI realtime session closed".into())),
        }
    }

    async fn next(&self) -> Result<Option<RealtimeEvent>, ModelError> {
        Ok(self.events.lock().await.recv().await)
    }

    async fn close(&self) -> Result<(), ModelError> {
        self.cancel.cancel();
        Ok(())
    }
}

fn audio_command(content: Content) -> Result<Value, ModelError> {
    let [eliteforge_agent_core::ContentBlock::Media { media }] = content.0.as_slice() else {
        return Err(ModelError::Fatal(
            "PersonAI realtime accepts one inline audio block".into(),
        ));
    };
    if !media.mime_type.starts_with("audio/") {
        return Err(ModelError::Fatal(
            "PersonAI realtime requires audio input".into(),
        ));
    }
    let MediaSource::Base64(encoded) = &media.source else {
        return Err(ModelError::Fatal(
            "PersonAI realtime requires inline base64 audio".into(),
        ));
    };
    Ok(json!({"type": "audio", "audio": encoded}))
}

fn map_event(message: &Value) -> Option<RealtimeEvent> {
    if message["type"] == "error" {
        return Some(RealtimeEvent::Error(
            message["message"]
                .as_str()
                .unwrap_or("PersonAI realtime error")
                .into(),
        ));
    }
    let payload = if message["type"] == "realtime_event" {
        &message["event"]
    } else {
        message
    };
    let text = payload["text"]
        .as_str()
        .or_else(|| payload["transcript"].as_str())
        .or_else(|| payload["delta"].as_str());
    text.map(|value| RealtimeEvent::Transcript(value.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AccountContext, PersonAiContextHandle, PersonAiModels, PersonAiServices};
    use crab_serverapi::base::AuthorizedClientContext;
    use eliteforge_agent_core::{ContentBlock, MediaContent, ModelChoice};
    use rsa::{RsaPrivateKey, pkcs8::EncodePublicKey};
    use tokio::net::TcpListener;

    #[test]
    fn maps_transcript_delta() {
        let event = json!({"type":"realtime_event","event":{"delta":"你好"}});
        assert!(
            matches!(map_event(&event), Some(RealtimeEvent::Transcript(text)) if text == "你好")
        );
    }

    #[tokio::test]
    async fn websocket_sends_audio_while_receiving_transcript() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let start = ws.next().await.unwrap().unwrap();
            assert!(matches!(start, Message::Text(_)));
            let audio = ws.next().await.unwrap().unwrap();
            assert!(matches!(audio, Message::Text(_)));
            ws.send(Message::Text(
                json!({
                    "type":"realtime_event", "event":{"delta":"transcribed"}
                })
                .to_string(),
            ))
            .await
            .unwrap();
            let _ = ws.next().await;
        });
        let private = RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
        let public = private
            .to_public_key()
            .to_public_key_pem(rsa::pkcs8::LineEnding::LF)
            .unwrap();
        let context = Arc::new(PersonAiContextHandle::default());
        context.publish(AccountContext {
            account_scope: "alice".into(),
            revision: 1,
            authorization: AuthorizedClientContext::new()
                .with_token("token")
                .with_client_id("client")
                .with_public_key(public)
                .with_product_code("product")
                .with_product_version("1.0.0"),
        });
        let services = Arc::new(PersonAiServices::new(&url).unwrap());
        let factory = PersonAiModels::new("personai".into(), &url, services, context).unwrap();
        let adapter = PersonAiRealtimeAdapter {
            binding: factory.bind_current().unwrap(),
            server_base_url: url,
        };
        let session = adapter
            .connect(
                "speech-auto",
                RealtimeRequest {
                    choice: ModelChoice::default(),
                    input: vec![Modality::Audio],
                    output: vec![Modality::Text],
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        session
            .send(RealtimeInput::Content(Content(vec![ContentBlock::Media {
                media: MediaContent {
                    mime_type: "audio/pcm".into(),
                    source: MediaSource::Base64("AA==".into()),
                    name: None,
                    estimated_tokens: Some(1),
                },
            }])))
            .await
            .unwrap();
        let received = tokio::time::timeout(std::time::Duration::from_secs(3), session.next())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(received, Some(RealtimeEvent::Transcript(text)) if text == "transcribed"));
        session.close().await.unwrap();
        server.await.unwrap();
    }
}

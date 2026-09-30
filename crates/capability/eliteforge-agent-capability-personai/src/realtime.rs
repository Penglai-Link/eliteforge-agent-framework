//! Full-duplex adapter for PersonAI's encrypted audio transcription WebSocket.
use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use crab_serverapi::{base::AuthorizedClientApi, crypto::ClientEnvelopeEncryptor};
use eliteforge_agent_core::{
    Content, MediaSource, Modality, ModelError, RealtimeEvent, RealtimeInput, RealtimeModel,
    RealtimeRequest, RealtimeSession,
};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
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
        let mut params = HashMap::new();
        params.insert("token".into(), account.token.clone().unwrap_or_default());
        params.insert(
            "client_id".into(),
            account.client_id.clone().unwrap_or_default(),
        );
        params.insert(
            "product_code".into(),
            account.product_code.clone().unwrap_or_default(),
        );
        params.insert(
            "product_version".into(),
            account.product_version.clone().unwrap_or_default(),
        );
        let api = AuthorizedClientApi::new(&self.server_base_url)
            .map_err(|error| ModelError::Fatal(error.to_string()))?;
        let url = api
            .build_websocket_url("/ws/audio/transcriptions", &params)
            .map_err(|error| ModelError::Fatal(error.to_string()))?;
        let (mut websocket, _) = tokio_tungstenite::connect_async(url)
            .await
            .map_err(|error| ModelError::Transient(error.to_string()))?;
        let public_key = account.public_key.clone().unwrap_or_default();
        let start = encrypted(
            &public_key,
            &json!({
                "type": "start", "language": "auto", "target_language": null,
                "prompt": null, "vad": true, "delay": null,
            }),
        )?;
        websocket
            .send(Message::Text(start.into()))
            .await
            .map_err(|error| ModelError::Transient(error.to_string()))?;
        let (command_tx, mut command_rx) = mpsc::channel::<Value>(32);
        let (event_tx, event_rx) = mpsc::channel::<RealtimeEvent>(64);
        let close = cancellation.child_token();
        let worker_cancel = close.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = worker_cancel.cancelled() => break,
                    command = command_rx.recv() => match command {
                        Some(command) => match encrypted(&public_key, &command) {
                            Ok(payload) => {
                                if let Err(error) = websocket.send(Message::Text(payload.into())).await {
                                    let _ = event_tx.send(RealtimeEvent::Error(error.to_string())).await;
                                    break;
                                }
                            }
                            Err(error) => {
                                let _ = event_tx.send(RealtimeEvent::Error(error.to_string())).await;
                                break;
                            }
                        },
                        None => break,
                    },
                    incoming = websocket.next() => match incoming {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                                if let Some(event) = map_event(&value) {
                                    if event_tx.send(event).await.is_err() { break; }
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) | None => break,
                        Some(Err(error)) => {
                            let _ = event_tx.send(RealtimeEvent::Error(error.to_string())).await;
                            break;
                        }
                        _ => {},
                    },
                }
            }
            let _ = websocket.close(None).await;
            let _ = event_tx.send(RealtimeEvent::Completed).await;
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
        self.commands
            .send(payload)
            .await
            .map_err(|_| ModelError::Transient("PersonAI realtime session closed".into()))
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

fn encrypted(public_key: &str, command: &Value) -> Result<String, ModelError> {
    let envelope = ClientEnvelopeEncryptor::encrypt(public_key, &command.to_string())
        .map_err(|error| ModelError::Fatal(error.to_string()))?;
    serde_json::to_string(&envelope).map_err(|error| ModelError::Fatal(error.to_string()))
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

    #[test]
    fn maps_transcript_delta() {
        let event = json!({"type":"realtime_event","event":{"delta":"你好"}});
        assert!(
            matches!(map_event(&event), Some(RealtimeEvent::Transcript(text)) if text == "你好")
        );
    }
}

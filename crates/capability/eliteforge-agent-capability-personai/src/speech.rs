//! Person-AI 语音适配：协议、加密、连接生命周期均由官方 SDK 持有。
use async_trait::async_trait;
use base64::Engine;
use crab_serverapi::{audio::AudioTranscriptionsClient, base::AuthorizedClientContext};
use eliteforge_agent_core::{
    MediaSource, ModelError, SpeechRecognitionModel, SpeechRecognitionRequest,
    SpeechRecognitionResponse,
};
use std::sync::Arc;

/// 为宿主捕获的一次授权快照提供语音能力；每次调用独占 SDK 会话。
pub struct PersonAiSpeechRecognitionModel {
    pub client: Arc<AudioTranscriptionsClient>,
    pub authorization: AuthorizedClientContext,
}

#[async_trait]
impl SpeechRecognitionModel for PersonAiSpeechRecognitionModel {
    async fn transcribe(
        &self,
        _: &str,
        request: SpeechRecognitionRequest,
    ) -> Result<SpeechRecognitionResponse, ModelError> {
        let model = request
            .choice
            .model
            .ok_or_else(|| ModelError::Fatal("speech model binding missing".into()))?;
        let MediaSource::Base64(encoded) = request.audio.source else {
            return Err(ModelError::Fatal(
                "PersonAI speech requires inline PCM WAV audio".into(),
            ));
        };
        if !matches!(
            request.audio.mime_type.as_str(),
            "audio/wav" | "audio/x-wav"
        ) {
            return Err(ModelError::Fatal(
                "PersonAI realtime transcription requires PCM WAV audio".into(),
            ));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| ModelError::Fatal(format!("invalid speech audio: {e}")))?;
        let result = self
            .client
            .transcribe_wav(&self.authorization, &bytes, request.language.as_deref())
            .await
            .map_err(|e| ModelError::Fatal(e.to_string()))?;
        Ok(SpeechRecognitionResponse {
            model,
            text: result.text,
            segments: Vec::new(),
        })
    }
}

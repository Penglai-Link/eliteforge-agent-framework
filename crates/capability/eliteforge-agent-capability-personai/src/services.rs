//! SDK clients exposed without making product account operations Agent tools.
use crab_serverapi::{
    ad::AdClient,
    agent_role::AgentRoleRepositoryClient,
    agent_site::AgentSiteClient,
    audio::{AudioFileTranscriptionClient, AudioTranscriptionsClient},
    auth::{AccountClient, LoginClient},
    checkin::CheckinClient,
    copyright::CopyrightClient,
    feedback::FeedbackClient,
    file::FileClient,
    gateway::{
        ChatCompletionsClient, EmbeddingsClient, RerankClient, ResponsesClient, SearchClient,
    },
    multimedia::MultimediaClient,
    notification::NotificationClient,
    relay::RelayClient,
    repository::MultiAgentRepositoryClient,
    skill::SkillRepositoryClient,
    skin::SkinMarketClient,
    version::VersionClient,
};
use std::sync::Arc;

pub struct PersonAiServices {
    pub login: Arc<LoginClient>,
    pub account: Arc<AccountClient>,
    pub checkin: Arc<CheckinClient>,
    pub chat: Arc<ChatCompletionsClient>,
    pub responses: Arc<ResponsesClient>,
    pub embeddings: Arc<EmbeddingsClient>,
    pub rerank: Arc<RerankClient>,
    pub search: Arc<SearchClient>,
    pub multimedia: Arc<MultimediaClient>,
    pub audio_file: Arc<AudioFileTranscriptionClient>,
    pub audio_realtime: Arc<AudioTranscriptionsClient>,
    pub skills: Arc<SkillRepositoryClient>,
    pub roles: Arc<AgentRoleRepositoryClient>,
    pub repositories: Arc<MultiAgentRepositoryClient>,
    pub sites: Arc<AgentSiteClient>,
    pub skins: Arc<SkinMarketClient>,
    pub files: Arc<FileClient>,
    pub copyright: Arc<CopyrightClient>,
    pub notifications: Arc<NotificationClient>,
    pub ads: Arc<AdClient>,
    pub feedback: Arc<FeedbackClient>,
    pub versions: Arc<VersionClient>,
    pub relay: Arc<RelayClient>,
}

impl PersonAiServices {
    pub fn new(url: &str) -> crab_serverapi::error::Result<Self> {
        Ok(Self {
            login: Arc::new(LoginClient::new(url)?),
            account: Arc::new(AccountClient::new(url)?),
            checkin: Arc::new(CheckinClient::new(url)?),
            chat: Arc::new(ChatCompletionsClient::new(url)?),
            responses: Arc::new(ResponsesClient::new(url)?),
            embeddings: Arc::new(EmbeddingsClient::new(url)?),
            rerank: Arc::new(RerankClient::new(url)?),
            search: Arc::new(SearchClient::new(url)?),
            multimedia: Arc::new(MultimediaClient::new(url)?),
            audio_file: Arc::new(AudioFileTranscriptionClient::new(url)?),
            audio_realtime: Arc::new(AudioTranscriptionsClient::new(url)?),
            skills: Arc::new(SkillRepositoryClient::new(url)?),
            roles: Arc::new(AgentRoleRepositoryClient::new(url)?),
            repositories: Arc::new(MultiAgentRepositoryClient::new(url)?),
            sites: Arc::new(AgentSiteClient::new(url)?),
            skins: Arc::new(SkinMarketClient::new(url)?),
            files: Arc::new(FileClient::new(url)?),
            copyright: Arc::new(CopyrightClient::new(url)?),
            notifications: Arc::new(NotificationClient::new(url)?),
            ads: Arc::new(AdClient::new(url)?),
            feedback: Arc::new(FeedbackClient::new(url)?),
            versions: Arc::new(VersionClient::new(url)?),
            relay: Arc::new(RelayClient::new(url)?),
        })
    }
}

/// SDK device fingerprint helpers remain explicitly host-side.
pub use crab_serverapi::device::{detect_device_id, validate_device_id};

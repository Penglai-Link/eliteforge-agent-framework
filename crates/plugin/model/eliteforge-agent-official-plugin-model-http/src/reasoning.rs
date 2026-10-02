use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReasoningEffort {
    #[serde(rename = "None")]
    None,
    #[serde(rename = "Minimal")]
    Minimal,
    #[serde(rename = "Low")]
    Low,
    #[serde(rename = "Medium")]
    Medium,
    #[serde(rename = "High")]
    High,
    #[serde(rename = "XHigh")]
    XHigh,
    #[serde(rename = "Max")]
    Max,
    #[serde(rename = "Ultra")]
    Ultra,
}

impl ReasoningEffort {
    pub const ALL: [Self; 8] = [
        Self::None,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
        Self::Ultra,
    ];

    pub const fn provider_value(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
        }
    }

    pub const fn storage_value(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Minimal => "Minimal",
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::XHigh => "XHigh",
            Self::Max => "Max",
            Self::Ultra => "Ultra",
        }
    }

    pub fn from_storage_value(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|effort| effort.storage_value() == value)
    }
}

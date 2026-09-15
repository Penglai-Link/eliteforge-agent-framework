//! Portable, bounded error reports. Raw execution errors remain available to
//! callers; exporting their text requires an explicit host redaction policy.
use crate::{AgentError, ModelError};
use serde::{Deserialize, Serialize};
use std::error::Error;

/// Return safe text or None to omit it. Called synchronously; implementations
/// must be fast and must not perform IO. Reports cap text AFTER redaction.
pub trait ErrorRedactor: Send + Sync {
    fn redact(&self, kind: &str, message: &str) -> Option<String>;
}

#[derive(Default)]
pub struct OmitErrorMessages;
impl ErrorRedactor for OmitErrorMessages {
    fn redact(&self, _: &str, _: &str) -> Option<String> {
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorReport {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub causes: Vec<ErrorReport>,
    /// Cleanup failures are separate from the original cause.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cleanup: Vec<ErrorReport>,
    pub truncated: bool,
}

impl ErrorReport {
    pub fn from_error(
        kind: &str,
        error: &(dyn Error + 'static),
        redactor: &dyn ErrorRedactor,
    ) -> Self {
        Self::build(kind, error, redactor, &mut 16)
    }

    fn build(
        kind: &str,
        error: &(dyn Error + 'static),
        redactor: &dyn ErrorRedactor,
        remaining: &mut usize,
    ) -> Self {
        *remaining -= 1;
        let message = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            redactor.redact(kind, &error.to_string())
        }))
        .ok()
        .flatten();
        let truncated = message.as_ref().is_some_and(|s| s.len() > 2048);
        let message = message.map(|mut s| {
            let mut end = s.len().min(2048);
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            s.truncate(end);
            s
        });
        let mut report = Self {
            kind: kind.into(),
            message,
            causes: vec![],
            cleanup: vec![],
            truncated,
        };
        if let Some(AgentError::WithCleanup { primary, cleanup }) =
            error.downcast_ref::<AgentError>()
        {
            for (error, target) in [
                (primary.as_ref(), &mut report.causes),
                (cleanup.as_ref(), &mut report.cleanup),
            ] {
                if *remaining == 0 {
                    report.truncated = true;
                } else {
                    target.push(Self::build(error.kind(), error, redactor, remaining));
                }
            }
        } else if let Some(source) = error.source() {
            if *remaining == 0 {
                report.truncated = true;
            } else {
                report
                    .causes
                    .push(Self::build("source", source, redactor, remaining));
            }
        }
        report.truncated |= report
            .causes
            .iter()
            .chain(&report.cleanup)
            .any(|cause| cause.truncated);
        report
    }
}

impl ModelError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::State(_) => "state",
            Self::Transient(_) => "transient",
            Self::ContextOverflow(_) => "context_overflow",
            Self::Fatal(_) => "model_fatal",
        }
    }
}

impl AgentError {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Plugin(_) => "plugin",
            Self::Capability(_) => "capability",
            Self::MissingService(_) => "missing_service",
            Self::InvalidConfiguration(_) => "configuration",
            Self::Closed => "closed",
            Self::SessionBusy(_) => "session_busy",
            Self::Cancelled => "cancelled",
            Self::Model(e) => e.kind(),
            Self::ConversationStore(_) => "conversation_store",
            Self::Context(crate::ContextError::BudgetExceeded(_)) => "context_budget",
            Self::Context(_) => "context",
            Self::ContextAssembly { source, .. } => source.kind(),
            Self::LongTermMemory(_) => "long_term_memory",
            Self::ToolSetup(_) => "tool_setup",
            Self::State(_) => "state",
            Self::Run(_) => "run",
            Self::WithCleanup { primary, .. } => primary.kind(),
        }
    }
    pub fn report(&self, redactor: &dyn ErrorRedactor) -> ErrorReport {
        ErrorReport::from_error(self.kind(), self, redactor)
    }
}

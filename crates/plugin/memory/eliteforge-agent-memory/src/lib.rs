//! Host-scoped memory contributors and explicit tools. Storage is supplied by the host.
mod components;
mod validation;
pub use components::*;

mod context;
pub use context::*;
mod compaction;
mod history;
mod policy;
mod short_term;
pub use compaction::*;
pub use eliteforge_agent_core::{
    ContextAssembler, ContextContributor, IndependentBlockBudget, RecentConversation,
    SelectContextEntries,
};
pub use history::*;
pub use policy::*;
pub use short_term::*;

pub use eliteforge_agent_context::{
    BoundMultimodalData, MediaAdmissionOutcome, MediaContextAudit, MediaContextReport,
    project_multimodal_data,
};

pub use eliteforge_agent_context::summary_byte_guard;
pub use eliteforge_agent_context::{BorrowHistoryForData, required_recent_turn_tokens};

pub use eliteforge_agent_context::{ImageEvidenceOptions, prepare_inline_image_evidence};

pub use eliteforge_agent_context::{PdfEvidenceOptions, RenderedPdfPage, render_pdf_evidence};

mod reduction_cache;
pub use reduction_cache::*;

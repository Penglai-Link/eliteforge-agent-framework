//! Opt-in context reduction and artifact offload, independent of host business state.
extern crate self as eliteforge_agent_context;

mod artifact;
mod compression;
mod summary;
pub use artifact::*;
pub use compression::*;
pub use summary::*;

mod media;
pub use media::*;

mod image_evidence;
pub use image_evidence::*;

mod pdf_evidence;
pub use pdf_evidence::*;

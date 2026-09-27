//! Opt-in context reduction and artifact offload, independent of host business state.
extern crate self as eliteforge_agent_context;

mod artifact;
mod compression;
mod summary;
pub use artifact::*;
pub use compression::*;
pub use summary::*;

mod sqlite_artifact;
pub use sqlite_artifact::*;

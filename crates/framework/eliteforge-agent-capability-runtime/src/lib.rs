//! Capability package installation, scoped process instances and explicit event routing.
mod directory;
mod host;
pub use directory::*;
mod manager;
mod manifest;
mod observation;
mod outputs;
pub use observation::*;
mod packaging;
mod pages;
pub use packaging::*;
mod process;
mod remote;
mod remote_context;
mod resource;
mod rpc;
mod sdk;
mod selection;
mod typed_sdk;
mod usage;
pub use manager::*;
pub use manifest::*;
pub use outputs::*;
pub use pages::*;
pub use process::*;
pub use remote::*;
pub use resource::*;
pub use rpc::*;
pub use sdk::*;
pub use selection::*;
pub use typed_sdk::*;
pub use usage::*;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("{0}")]
    Invalid(String),
    #[error("capability package is busy: {0:?}")]
    Busy(Vec<UsageBlocker>),
    #[error("remote error: {0:?}")]
    Fault(RpcFault),
    #[error("capability package RPC failed: {0}")]
    Rpc(String),
    #[error("execution result is unknown: {0}")]
    Indeterminate(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
pub type RuntimeResult<T> = Result<T, RuntimeError>;

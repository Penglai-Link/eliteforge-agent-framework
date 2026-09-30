use eliteforge_agent_core::MemoryError;

pub(crate) fn validate_namespace(namespace: &str) -> Result<(), MemoryError> {
    if namespace.trim().is_empty() || namespace.len() > 512 {
        return Err(MemoryError::Invalid(
            "namespace must contain 1..=512 bytes".into(),
        ));
    }
    Ok(())
}

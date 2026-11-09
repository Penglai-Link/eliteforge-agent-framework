use eliteforge_agent_core::AgentError;
use jsonschema::{ValidationError, error::ValidationErrorKind};
use serde_json::Value;

use super::{VERSION, invalid};

const OPERATIONS: [&str; 4] = [
    "createSurface",
    "updateComponents",
    "updateDataModel",
    "deleteSurface",
];

/// Check the envelope before catalog lookup so flattened payloads get an actionable error.
pub(super) fn envelope_operation(
    message: &Value,
    index: usize,
) -> Result<(&str, &Value), AgentError> {
    let example = if message.get("components").is_some() {
        r#"{"version":"v0.9.1","updateComponents":{"surfaceId":"form","components":[...]}}"#
    } else {
        r#"{"version":"v0.9.1","createSurface":{"surfaceId":"form","catalogId":"<negotiated catalog>"}}"#
    };
    let object = message.as_object().ok_or_else(|| {
        invalid(format!(
            "messages[{index}]: expected a complete A2UI envelope, for example {example}"
        ))
    })?;
    if object.contains_key("surfaceId") || object.contains_key("components") {
        return Err(invalid(format!(
            "messages[{index}]: surfaceId and operation fields belong inside createSurface/updateComponents/updateDataModel/deleteSurface, not at the message root. Expected {example}"
        )));
    }
    if message["version"] != VERSION {
        return Err(invalid(format!(
            "messages[{index}]/version: expected {VERSION}"
        )));
    }
    let mut fields = object.iter().filter(|(key, _)| key.as_str() != "version");
    let (operation, payload) = fields.next().ok_or_else(|| {
        invalid(format!(
            "messages[{index}]: missing operation; expected {example}"
        ))
    })?;
    if fields.next().is_some() || !OPERATIONS.contains(&operation.as_str()) {
        return Err(invalid(format!(
            "messages[{index}]: expected exactly one of createSurface, updateComponents, updateDataModel, deleteSurface alongside version; keep mode and responseSchema outside messages"
        )));
    }
    if !payload.is_object() {
        return Err(invalid(format!(
            "messages[{index}]/{operation}: expected an object"
        )));
    }
    Ok((operation, payload))
}

pub(super) fn envelope_error_details(error: &ValidationError<'_>, index: usize) -> Vec<String> {
    match error.kind() {
        ValidationErrorKind::OneOfNotValid { context } | ValidationErrorKind::AnyOf { context } => {
            let component_path = format!("{}/component", error.instance_path());
            let component = error.instance().get("component");
            let operation = OPERATIONS
                .iter()
                .find(|name| error.instance().get(**name).is_some());
            context
                .iter()
                // Prefer the branch matching the operation/component discriminator, even
                // when that branch has more errors than an unrelated component schema.
                .min_by_key(|branch| {
                    let mismatch = branch.iter().any(|child| match child.kind() {
                        ValidationErrorKind::Constant { expected_value } => {
                            child.instance_path().as_str() == component_path
                                && component.is_some_and(|value| value != expected_value)
                        }
                        ValidationErrorKind::Required { property } => {
                            child.instance_path() == error.instance_path()
                                && property.as_str().is_some_and(|name| {
                                    operation.is_some_and(|selected| name != *selected)
                                        && OPERATIONS.contains(&name)
                                })
                        }
                        _ => false,
                    });
                    (mismatch, branch.len())
                })
                .into_iter()
                .flatten()
                .flat_map(|child| envelope_error_details(child, index))
                .take(16)
                .collect()
        }
        _ => vec![format!(
            "messages[{index}]{}: {error}",
            error.instance_path()
        )],
    }
}

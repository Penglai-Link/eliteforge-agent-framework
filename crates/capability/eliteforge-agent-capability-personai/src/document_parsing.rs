//! PersonAI wire modes belong to this capability pack, never Framework dispatch.
use crate::sdk::document_parse::ParseMode;

/// Host-selected intent; this pack does not inspect files or select quality.
pub enum PersonAiDocumentParsingIntent {
    Native,
    Recognition,
    Layout,
}

pub fn personai_document_parse_mode(method: PersonAiDocumentParsingIntent) -> ParseMode {
    match method {
        PersonAiDocumentParsingIntent::Native => ParseMode::Text,
        PersonAiDocumentParsingIntent::Recognition => ParseMode::Ocr,
        PersonAiDocumentParsingIntent::Layout => ParseMode::Precise,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maps_all_generic_methods_to_native_sdk_modes() {
        assert_eq!(
            personai_document_parse_mode(PersonAiDocumentParsingIntent::Native),
            ParseMode::Text
        );
        assert_eq!(
            personai_document_parse_mode(PersonAiDocumentParsingIntent::Recognition),
            ParseMode::Ocr
        );
        assert_eq!(
            personai_document_parse_mode(PersonAiDocumentParsingIntent::Layout),
            ParseMode::Precise
        );
    }
}

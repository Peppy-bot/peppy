//! The descriptions of documents, of their members and of their fields.
//!
//! A description is prose for the readers of a document: the people who
//! choose a contract, a pairing, a node or a launcher, and the models that
//! call its members. Each document of these kinds has a
//! [`DocumentDescription`]; each member of a contract, each topic of a
//! pairing and each member that a node declares itself has a
//! [`Description`]; a field of a message format can have one
//! (`$description`). Peppy reads no rule from a description: enums, marks
//! and failure kinds stay the rules that the checkers read.

use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;

/// One text that is not blank: the description of a member or of a field,
/// or one paragraph of the description of a document.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Description(String);

/// Why a text is not a [`Description`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the description is blank; write what it describes")]
pub struct BlankDescription;

impl Description {
    pub fn new(text: impl Into<String>) -> Result<Self, BlankDescription> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(BlankDescription);
        }
        Ok(Self(text))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Description {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Description {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::new(text).map_err(serde::de::Error::custom)
    }
}

/// The description of a document: its paragraphs, at least one, in the
/// order they are written. The first paragraph is the summary of the
/// document, which `peppy repo search` prints.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct DocumentDescription(Vec<Description>);

/// Why a list of paragraphs is not a [`DocumentDescription`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the description lists no paragraph; its first paragraph says what the document is for")]
pub struct NoParagraph;

impl DocumentDescription {
    pub fn new(paragraphs: Vec<Description>) -> Result<Self, NoParagraph> {
        if paragraphs.is_empty() {
            return Err(NoParagraph);
        }
        Ok(Self(paragraphs))
    }

    /// The first paragraph: what the document is for, in short.
    pub fn summary(&self) -> &Description {
        &self.0[0]
    }

    pub fn paragraphs(&self) -> &[Description] {
        &self.0
    }
}

impl<'de> Deserialize<'de> for DocumentDescription {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let paragraphs = Vec::<Description>::deserialize(deserializer)?;
        Self::new(paragraphs).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_description_is_a_text_that_is_not_blank() {
        assert_eq!(
            Description::new("Move the arm.").map(|d| d.to_string()),
            Ok("Move the arm.".to_owned())
        );
        for blank in ["", " ", "\n\t "] {
            assert_eq!(Description::new(blank), Err(BlankDescription), "{blank:?}");
        }
    }

    #[test]
    fn a_blank_description_does_not_parse() {
        let error = serde_json::from_str::<Description>(r#""  ""#).unwrap_err();
        assert!(error.to_string().contains("blank"), "{error}");
    }

    #[test]
    fn the_summary_of_a_document_is_its_first_paragraph() {
        let description: DocumentDescription =
            serde_json::from_str(r#"["What it is for.", "More about it."]"#).unwrap();
        assert_eq!(description.summary().as_str(), "What it is for.");
        assert_eq!(description.paragraphs().len(), 2);
    }

    #[test]
    fn a_document_description_has_a_paragraph_and_no_blank_one() {
        let error = serde_json::from_str::<DocumentDescription>("[]").unwrap_err();
        assert!(error.to_string().contains("no paragraph"), "{error}");
        let error =
            serde_json::from_str::<DocumentDescription>(r#"["What it is for.", ""]"#).unwrap_err();
        assert!(error.to_string().contains("blank"), "{error}");
    }

    #[test]
    fn a_document_description_is_a_list_and_not_one_string() {
        assert!(serde_json::from_str::<DocumentDescription>(r#""What it is for.""#).is_err());
    }

    #[test]
    fn a_document_description_serializes_as_its_list_of_paragraphs() {
        let text = r#"["What it is for.","More about it."]"#;
        let description: DocumentDescription = serde_json::from_str(text).unwrap();
        assert_eq!(serde_json::to_string(&description).unwrap(), text);
    }
}

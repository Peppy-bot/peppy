//! Rules a message format satisfies beyond what parsing establishes.
//!
//! Parsing pins the DSL's shape: no empty format, `$optional` only on
//! pointer-backed root fields, no nested arrays. Two further rules come from
//! what the wire encoding and the generated code can carry, and every
//! consumer that derives something from a format (the code generators, the
//! public MCP schema mapping) applies them through the methods here, so a
//! format one of them accepts is a format all of them accept:
//!
//! * a payload field cannot take a name reserved by transport metadata, at
//!   any depth;
//! * a fixed-length (`$length`) array holds scalar items only, because its
//!   fixed layout needs items of one fixed width.

use super::types::{MessageFormat, SchemaType};
use crate::common::type_token_name;

/// Payload field names reserved by transport metadata.
const RESERVED_FIELD_NAMES: &[&str] = &["instance_id"];

/// Why a parsed message format cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FormatRuleViolation {
    /// A payload field carries a name reserved by transport metadata.
    /// `path` is the dotted path of the field from the format's root.
    #[error("field name `{field}` is reserved by transport metadata (at `{path}`)")]
    ReservedFieldName { field: String, path: String },
    /// A `$length` array's items are not scalars. `item` is the offending
    /// item shape: `object` or the item's type token.
    #[error("unsupported fixed-length array item type `{item}` in field `{field}`")]
    UnsupportedFixedArrayItemType { field: String, item: &'static str },
}

impl MessageFormat {
    /// Refuses a payload field named like transport metadata, at any depth.
    pub fn check_reserved_field_names(&self) -> Result<(), FormatRuleViolation> {
        check_field_names(self.0.iter(), "")
    }

    /// Refuses a `$length` array whose items are not scalars, at any depth.
    pub fn check_fixed_length_array_items(&self) -> Result<(), FormatRuleViolation> {
        for (field_name, schema) in &self.0 {
            check_fixed_array(schema, field_name)?;
        }
        Ok(())
    }
}

fn check_field_names<'a>(
    fields: impl IntoIterator<Item = (&'a String, &'a SchemaType)>,
    parent_path: &str,
) -> Result<(), FormatRuleViolation> {
    for (field_name, schema) in fields {
        let path = if parent_path.is_empty() {
            field_name.clone()
        } else {
            format!("{parent_path}.{field_name}")
        };
        if RESERVED_FIELD_NAMES.contains(&field_name.as_str()) {
            return Err(FormatRuleViolation::ReservedFieldName {
                field: field_name.clone(),
                path,
            });
        }
        match schema {
            SchemaType::Object(object) => check_field_names(object.fields.iter(), &path)?,
            SchemaType::Array(array) => {
                if let SchemaType::Object(object) = array.items.as_ref() {
                    check_field_names(object.fields.iter(), &path)?;
                }
            }
            SchemaType::Type(_) | SchemaType::Primitive(_) => {}
        }
    }
    Ok(())
}

fn check_fixed_array(schema: &SchemaType, path: &str) -> Result<(), FormatRuleViolation> {
    match schema {
        SchemaType::Array(array) => {
            if array.length.is_some() {
                // Parsing already refuses arrays as array items, so the items
                // are either an object or a single token.
                let item = match array.items.as_ref().as_type_token() {
                    Some(token) if token.is_scalar() => return Ok(()),
                    Some(token) => type_token_name(token),
                    None => "object",
                };
                return Err(FormatRuleViolation::UnsupportedFixedArrayItemType {
                    field: path.to_string(),
                    item,
                });
            }
            check_fixed_array(array.items.as_ref(), path)
        }
        SchemaType::Object(object) => {
            for (field_name, nested) in &object.fields {
                check_fixed_array(nested, &format!("{path}.{field_name}"))?;
            }
            Ok(())
        }
        SchemaType::Type(_) | SchemaType::Primitive(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(json5: &str) -> MessageFormat {
        serde_json5::from_str(json5).expect("format parses")
    }

    #[test]
    fn a_reserved_root_field_name_is_refused() {
        let violation = format(r#"{ instance_id: "string", value: "u8" }"#)
            .check_reserved_field_names()
            .expect_err("instance_id is reserved");
        assert_eq!(
            violation,
            FormatRuleViolation::ReservedFieldName {
                field: "instance_id".to_string(),
                path: "instance_id".to_string(),
            }
        );
        assert_eq!(
            violation.to_string(),
            "field name `instance_id` is reserved by transport metadata (at `instance_id`)"
        );
    }

    #[test]
    fn a_reserved_nested_field_name_reports_its_path() {
        let violation = format(r#"{ header: { $type: "object", instance_id: "string" } }"#)
            .check_reserved_field_names()
            .expect_err("nested names are checked too");
        assert_eq!(
            violation,
            FormatRuleViolation::ReservedFieldName {
                field: "instance_id".to_string(),
                path: "header.instance_id".to_string(),
            }
        );

        let violation = format(
            r#"{ samples: { $type: "array", $items: { $type: "object", instance_id: "u8" } } }"#,
        )
        .check_reserved_field_names()
        .expect_err("array item objects are checked too");
        assert_eq!(
            violation,
            FormatRuleViolation::ReservedFieldName {
                field: "instance_id".to_string(),
                path: "samples.instance_id".to_string(),
            }
        );
    }

    #[test]
    fn ordinary_names_pass_the_reserved_check() {
        format(r#"{ instance: "u8", header: { $type: "object", id: "string" } }"#)
            .check_reserved_field_names()
            .expect("nothing reserved");
    }

    #[test]
    fn a_fixed_array_of_strings_is_refused() {
        let violation = format(r#"{ labels: { $type: "array", $items: "string", $length: 3 } }"#)
            .check_fixed_length_array_items()
            .expect_err("fixed arrays hold scalars only");
        assert_eq!(
            violation,
            FormatRuleViolation::UnsupportedFixedArrayItemType {
                field: "labels".to_string(),
                item: "string",
            }
        );
    }

    #[test]
    fn a_fixed_array_of_objects_is_refused_with_its_nested_path() {
        let violation = format(
            r#"{
                frame: {
                    $type: "object",
                    corners: {
                        $type: "array",
                        $items: { $type: "object", name: "string" },
                        $length: 4,
                    },
                },
            }"#,
        )
        .check_fixed_length_array_items()
        .expect_err("fixed arrays hold scalars only");
        assert_eq!(
            violation,
            FormatRuleViolation::UnsupportedFixedArrayItemType {
                field: "frame.corners".to_string(),
                item: "object",
            }
        );
    }

    #[test]
    fn fixed_scalar_arrays_and_variable_arrays_pass() {
        format(
            r#"{
                samples: { $type: "array", $items: "i32", $length: 4 },
                names: { $type: "array", $items: "string" },
                poses: { $type: "array", $items: { $type: "object", x: "f64" } },
            }"#,
        )
        .check_fixed_length_array_items()
        .expect("scalar fixed arrays and variable arrays are supported");
    }
}

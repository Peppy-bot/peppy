use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// A flexible value to hold arbitrary JSON5 content (runtime values only)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(untagged)]
pub enum AnyType {
    #[default]
    Null,
    Bool(bool),
    /// A plain string value
    String(String),
    Array(Vec<AnyType>),
    Object(BTreeMap<String, AnyType>),

    // Numeric values: prefer signed, then unsigned, then float
    Int(i64),
    UInt(u64),
    Float(f64),
}

impl AnyType {
    /// Returns a human-readable name for this value's type
    pub fn type_name(&self) -> &'static str {
        match self {
            AnyType::Null => "null",
            AnyType::Bool(_) => "bool",
            AnyType::String(_) => "string",
            AnyType::Array(_) => "array",
            AnyType::Object(_) => "object",
            AnyType::Int(_) => "int",
            AnyType::UInt(_) => "uint",
            AnyType::Float(_) => "float",
        }
    }

    /// Validates that this runtime value matches a type specification.
    ///
    /// The type specification can be:
    /// - A string like "f32", "bool", "string", etc.
    /// - An object with `$type: "array"` and `$items: <type_spec>`
    /// - An object with `$type: "object"` and field type specifications
    ///
    /// Returns `Ok(())` if the value matches, or an error with the path to the mismatch.
    pub fn matches_type_spec(&self, type_spec: &AnyType, path: &str) -> Result<(), TypeMismatch> {
        match type_spec {
            // Simple type specification as a string
            AnyType::String(type_name) => self.matches_simple_type(type_name, path),
            // Complex type specification (array or object with $type)
            AnyType::Object(spec_map) => {
                if let Some(AnyType::String(type_kind)) = spec_map.get("$type") {
                    match type_kind.as_str() {
                        "array" => self.matches_array_spec(spec_map, path),
                        "object" => self.matches_object_spec(spec_map, path),
                        _ => Err(TypeMismatch {
                            path: path.to_string(),
                            expected: format!("valid $type (array or object), got '{}'", type_kind),
                            actual: self.type_name().to_string(),
                        }),
                    }
                } else {
                    // Inline object type spec without $type
                    self.matches_inline_object_spec(spec_map, path)
                }
            }
            _ => Err(TypeMismatch {
                path: path.to_string(),
                expected: "type specification (string or object)".to_string(),
                actual: format!("invalid spec: {}", type_spec.type_name()),
            }),
        }
    }

    fn matches_simple_type(&self, expected_type: &str, path: &str) -> Result<(), TypeMismatch> {
        let matches = match expected_type {
            "bool" => matches!(self, AnyType::Bool(_)),
            "string" | "str" => matches!(self, AnyType::String(_)),
            "i8" | "i16" | "i32" | "i64" | "int" => matches!(self, AnyType::Int(_)),
            "u8" | "u16" | "u32" | "u64" | "uint" => match self {
                AnyType::UInt(_) => true,
                AnyType::Int(i) => *i >= 0,
                _ => false,
            },
            "f32" | "f64" | "float" | "double" => {
                matches!(self, AnyType::Float(_) | AnyType::Int(_) | AnyType::UInt(_))
            }
            "null" => matches!(self, AnyType::Null),
            _ => {
                return Err(TypeMismatch {
                    path: path.to_string(),
                    expected: format!("known type, got '{}'", expected_type),
                    actual: self.type_name().to_string(),
                });
            }
        };

        if matches {
            Ok(())
        } else {
            Err(TypeMismatch {
                path: path.to_string(),
                expected: expected_type.to_string(),
                actual: self.type_name().to_string(),
            })
        }
    }

    fn matches_array_spec(
        &self,
        spec_map: &BTreeMap<String, AnyType>,
        path: &str,
    ) -> Result<(), TypeMismatch> {
        let arr = match self {
            AnyType::Array(arr) => arr,
            _ => {
                return Err(TypeMismatch {
                    path: path.to_string(),
                    expected: "array".to_string(),
                    actual: self.type_name().to_string(),
                });
            }
        };

        if let Some(item_type) = spec_map.get("$items") {
            for (i, item) in arr.iter().enumerate() {
                let item_path = format!("{}[{}]", path, i);
                item.matches_type_spec(item_type, &item_path)?;
            }
        }

        Ok(())
    }

    fn matches_object_spec(
        &self,
        spec_map: &BTreeMap<String, AnyType>,
        path: &str,
    ) -> Result<(), TypeMismatch> {
        let obj = match self {
            AnyType::Object(obj) => obj,
            _ => {
                return Err(TypeMismatch {
                    path: path.to_string(),
                    expected: "object".to_string(),
                    actual: self.type_name().to_string(),
                });
            }
        };

        for (key, field_value) in obj {
            if key.starts_with('$') {
                continue; // Skip meta fields like $type
            }
            let field_type = spec_map.get(key).ok_or_else(|| TypeMismatch {
                path: format!("{}.{}", path, key),
                expected: "field defined in type spec".to_string(),
                actual: "missing field definition".to_string(),
            })?;
            let field_path = format!("{}.{}", path, key);
            field_value.matches_type_spec(field_type, &field_path)?;
        }

        Ok(())
    }

    fn matches_inline_object_spec(
        &self,
        spec_map: &BTreeMap<String, AnyType>,
        path: &str,
    ) -> Result<(), TypeMismatch> {
        let obj = match self {
            AnyType::Object(obj) => obj,
            _ => {
                return Err(TypeMismatch {
                    path: path.to_string(),
                    expected: "object".to_string(),
                    actual: self.type_name().to_string(),
                });
            }
        };

        for (key, field_value) in obj {
            let field_type = spec_map.get(key).ok_or_else(|| TypeMismatch {
                path: format!("{}.{}", path, key),
                expected: "field defined in type spec".to_string(),
                actual: "missing field definition".to_string(),
            })?;
            let field_path = format!("{}.{}", path, key);
            field_value.matches_type_spec(field_type, &field_path)?;
        }

        Ok(())
    }
}

/// Error returned when a runtime value doesn't match its type specification
#[derive(Debug, Clone, PartialEq)]
pub struct TypeMismatch {
    pub path: String,
    pub expected: String,
    pub actual: String,
}

impl std::fmt::Display for TypeMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "type mismatch at `{}`: expected `{}`, got `{}`",
            self.path, self.expected, self.actual
        )
    }
}

impl std::error::Error for TypeMismatch {}

// Node parameters with open-ended structure
pub type NodeParameters = BTreeMap<String, AnyType>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_simple_bool() {
        let value = AnyType::Bool(true);
        let spec = AnyType::String("bool".to_string());
        assert!(value.matches_type_spec(&spec, "test").is_ok());
    }

    #[test]
    fn matches_simple_string() {
        let value = AnyType::String("hello".to_string());
        let spec = AnyType::String("string".to_string());
        assert!(value.matches_type_spec(&spec, "test").is_ok());
    }

    #[test]
    fn matches_simple_int() {
        let value = AnyType::Int(42);
        let spec = AnyType::String("i64".to_string());
        assert!(value.matches_type_spec(&spec, "test").is_ok());
    }

    #[test]
    fn matches_simple_float() {
        let value = AnyType::Float(3.14);
        let spec = AnyType::String("f32".to_string());
        assert!(value.matches_type_spec(&spec, "test").is_ok());
    }

    #[test]
    fn int_matches_float_spec() {
        let value = AnyType::Int(42);
        let spec = AnyType::String("f64".to_string());
        assert!(value.matches_type_spec(&spec, "test").is_ok());
    }

    #[test]
    fn positive_int_matches_uint_spec() {
        let value = AnyType::Int(42);
        let spec = AnyType::String("u32".to_string());
        assert!(value.matches_type_spec(&spec, "test").is_ok());
    }

    #[test]
    fn negative_int_fails_uint_spec() {
        let value = AnyType::Int(-5);
        let spec = AnyType::String("u32".to_string());
        let result = value.matches_type_spec(&spec, "test");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.path, "test");
        assert_eq!(err.expected, "u32");
        assert_eq!(err.actual, "int");
    }

    #[test]
    fn fails_type_mismatch() {
        let value = AnyType::String("not a bool".to_string());
        let spec = AnyType::String("bool".to_string());
        let result = value.matches_type_spec(&spec, "field");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.path, "field");
        assert_eq!(err.expected, "bool");
        assert_eq!(err.actual, "string");
    }

    #[test]
    fn matches_array_spec() {
        let value = AnyType::Array(vec![
            AnyType::String("a".to_string()),
            AnyType::String("b".to_string()),
        ]);
        let spec = AnyType::Object(BTreeMap::from([
            ("$type".to_string(), AnyType::String("array".to_string())),
            ("$items".to_string(), AnyType::String("string".to_string())),
        ]));
        assert!(value.matches_type_spec(&spec, "flags").is_ok());
    }

    #[test]
    fn fails_array_item_type_mismatch() {
        let value = AnyType::Array(vec![AnyType::String("a".to_string()), AnyType::Int(42)]);
        let spec = AnyType::Object(BTreeMap::from([
            ("$type".to_string(), AnyType::String("array".to_string())),
            ("$items".to_string(), AnyType::String("string".to_string())),
        ]));
        let result = value.matches_type_spec(&spec, "flags");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.path, "flags[1]");
        assert_eq!(err.expected, "string");
        assert_eq!(err.actual, "int");
    }

    #[test]
    fn matches_object_spec() {
        let value = AnyType::Object(BTreeMap::from([
            ("enabled".to_string(), AnyType::Bool(true)),
            ("gain".to_string(), AnyType::Int(10)),
        ]));
        let spec = AnyType::Object(BTreeMap::from([
            ("$type".to_string(), AnyType::String("object".to_string())),
            ("enabled".to_string(), AnyType::String("bool".to_string())),
            ("gain".to_string(), AnyType::String("i64".to_string())),
        ]));
        assert!(value.matches_type_spec(&spec, "nested").is_ok());
    }

    #[test]
    fn fails_object_field_type_mismatch() {
        let value = AnyType::Object(BTreeMap::from([
            ("enabled".to_string(), AnyType::String("yes".to_string())),
            ("gain".to_string(), AnyType::Int(10)),
        ]));
        let spec = AnyType::Object(BTreeMap::from([
            ("$type".to_string(), AnyType::String("object".to_string())),
            ("enabled".to_string(), AnyType::String("bool".to_string())),
            ("gain".to_string(), AnyType::String("i64".to_string())),
        ]));
        let result = value.matches_type_spec(&spec, "nested");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.path, "nested.enabled");
        assert_eq!(err.expected, "bool");
        assert_eq!(err.actual, "string");
    }

    #[test]
    fn fails_unknown_type_spec() {
        let value = AnyType::Int(42);
        let spec = AnyType::String("unknown_type".to_string());
        let result = value.matches_type_spec(&spec, "field");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.expected.contains("known type"));
    }

    #[test]
    fn type_mismatch_display() {
        let err = TypeMismatch {
            path: "config.timeout".to_string(),
            expected: "u32".to_string(),
            actual: "string".to_string(),
        };
        assert_eq!(
            format!("{}", err),
            "type mismatch at `config.timeout`: expected `u32`, got `string`"
        );
    }
}

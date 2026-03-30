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
            // "time" accepts integers (timestamps) or unsigned integers
            "time" => matches!(self, AnyType::Int(_) | AnyType::UInt(_)),
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

/// Type alias for parameter type specifications declared in `peppy.json5`.
///
/// Each key is a parameter name and each value describes the expected type
/// (e.g. `AnyType::String("f64")` or an object/array type spec).
pub type ParameterSchema = BTreeMap<String, AnyType>;

/// Unvalidated node arguments deserialized from a runtime or deployment config.
///
/// Use [`RawNodeArguments::into_resolved`] to validate against a
/// [`ParameterSchema`] and obtain a [`NodeArguments`] value.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct RawNodeArguments(BTreeMap<String, AnyType>);

impl RawNodeArguments {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    pub fn insert(&mut self, key: String, value: AnyType) -> Option<AnyType> {
        self.0.insert(key, value)
    }

    pub fn get(&self, key: &str) -> Option<&AnyType> {
        self.0.get(key)
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.0.keys()
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &AnyType)> {
        self.0.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn as_inner(&self) -> &BTreeMap<String, AnyType> {
        &self.0
    }

    pub fn into_inner(self) -> BTreeMap<String, AnyType> {
        self.0
    }

    /// Resolve a dot-separated path (e.g. `"video.device_path"`) against these
    /// arguments, returning the leaf [`AnyType`] value if found.
    pub fn resolve_path(&self, dot_path: &str) -> Option<&AnyType> {
        resolve_parameter_path(&self.0, dot_path)
    }

    /// Validate these raw arguments against a [`ParameterSchema`] and produce
    /// a [`NodeArguments`] value.
    ///
    /// Checks that:
    /// 1. Every key declared in the schema is present in the arguments.
    /// 2. Every argument value matches the type declared in the schema.
    /// 3. No unknown keys (absent from the schema) are present.
    pub fn into_resolved(
        self,
        schema: &ParameterSchema,
    ) -> Result<NodeArguments, NodeArgumentsError> {
        // 1. Check all schema keys are present.
        let missing: Vec<String> = schema
            .keys()
            .filter(|k| !self.0.contains_key(k.as_str()))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(NodeArgumentsError::MissingParameters(missing));
        }

        // 2. Validate types of each runtime arg against schema.
        for (key, value) in &self.0 {
            let type_spec = schema
                .get(key)
                .ok_or_else(|| NodeArgumentsError::UnknownParameter { key: key.clone() })?;
            value
                .matches_type_spec(type_spec, key)
                .map_err(NodeArgumentsError::TypeMismatch)?;
        }

        Ok(NodeArguments(self.0))
    }
}

impl<const N: usize> From<[(String, AnyType); N]> for RawNodeArguments {
    fn from(arr: [(String, AnyType); N]) -> Self {
        Self(BTreeMap::from(arr))
    }
}

impl From<BTreeMap<String, AnyType>> for RawNodeArguments {
    fn from(map: BTreeMap<String, AnyType>) -> Self {
        Self(map)
    }
}

/// Node arguments that have passed validation against the manifest spec.
///
/// This type cannot be constructed directly — it is only produced by
/// [`RawNodeArguments::into_resolved`]. The inner data is not accessible;
/// consumers must parse into a typed struct via
/// [`peppylib::config::deserialize_parameters`].
#[derive(Clone, Debug, Serialize)]
pub struct NodeArguments(BTreeMap<String, AnyType>);

/// Error produced when [`RawNodeArguments::into_resolved`] fails.
#[derive(Debug)]
pub enum NodeArgumentsError {
    /// One or more runtime argument values do not match the schema types.
    TypeMismatch(TypeMismatch),
    /// One or more parameters declared in the schema are missing from the arguments.
    MissingParameters(Vec<String>),
    /// An argument key is not declared in the schema.
    UnknownParameter { key: String },
}

impl std::fmt::Display for NodeArgumentsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TypeMismatch(tm) => write!(f, "{tm}"),
            Self::MissingParameters(keys) => {
                write!(f, "missing parameter(s): {}", keys.join(", "))
            }
            Self::UnknownParameter { key } => {
                write!(f, "unknown parameter `{key}` not declared in schema")
            }
        }
    }
}

impl std::error::Error for NodeArgumentsError {}

fn is_array_parameter_schema(map: &BTreeMap<String, AnyType>) -> bool {
    matches!(
        map.get("type"),
        Some(AnyType::String(kind)) if kind.eq_ignore_ascii_case("array")
    )
}

/// Validates that instance parameter values match the types declared in a parameter schema.
/// Recursively walks through nested objects to validate each leaf value.
///
/// Returns `Ok(())` if all values match, or the first `TypeMismatch` found.
pub fn validate_parameter_types(
    instance_params: &BTreeMap<String, AnyType>,
    manifest_params: &BTreeMap<String, AnyType>,
    prefix: &str,
) -> std::result::Result<(), TypeMismatch> {
    for (key, instance_value) in instance_params {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{}.{}", prefix, key)
        };

        let Some(manifest_value) = manifest_params.get(key) else {
            continue;
        };

        match (instance_value, manifest_value) {
            // Both are objects - recurse into nested structure
            (AnyType::Object(inst_map), AnyType::Object(man_map)) => {
                if is_array_parameter_schema(man_map) {
                    return Err(TypeMismatch {
                        path,
                        expected: "array".to_string(),
                        actual: "object".to_string(),
                    });
                }
                validate_parameter_types(inst_map, man_map, &path)?;
            }
            // Manifest declares a type (string like "f32", "bool", etc.)
            (instance_value, AnyType::String(_)) => {
                instance_value.matches_type_spec(manifest_value, &path)?;
            }
            // Manifest declares an object schema but instance provides a non-object
            (instance_value, AnyType::Object(man_map)) => {
                if is_array_parameter_schema(man_map) {
                    if !matches!(instance_value, AnyType::Array(_)) {
                        return Err(TypeMismatch {
                            path,
                            expected: "array".to_string(),
                            actual: instance_value.type_name().to_string(),
                        });
                    }
                    if let (AnyType::Array(items), Some(item_spec)) =
                        (instance_value, man_map.get("items"))
                    {
                        for (i, item) in items.iter().enumerate() {
                            let item_path = format!("{}[{}]", path, i);
                            item.matches_type_spec(item_spec, &item_path)?;
                        }
                    }
                } else {
                    return Err(TypeMismatch {
                        path,
                        expected: "object".to_string(),
                        actual: instance_value.type_name().to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Resolve a dot-path (e.g., `"video.device_path"`) against a parameter map,
/// returning the leaf `AnyType` value if found.
///
/// Descends into `AnyType::Object` values at each segment boundary.
pub fn resolve_parameter_path<'a>(
    parameters: &'a BTreeMap<String, AnyType>,
    dot_path: &str,
) -> Option<&'a AnyType> {
    let mut segments = dot_path.split('.');
    let first = segments.next()?;
    let mut current = parameters.get(first)?;

    for segment in segments {
        match current {
            AnyType::Object(map) => {
                current = map.get(segment)?;
            }
            _ => return None,
        }
    }

    Some(current)
}

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
        let value = AnyType::Float(1.0);
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
    fn matches_time_spec_with_int() {
        let value = AnyType::Int(1696285145999);
        let spec = AnyType::String("time".to_string());
        assert!(value.matches_type_spec(&spec, "test").is_ok());
    }

    #[test]
    fn matches_time_spec_with_uint() {
        let value = AnyType::UInt(1696285145999);
        let spec = AnyType::String("time".to_string());
        assert!(value.matches_type_spec(&spec, "test").is_ok());
    }

    #[test]
    fn string_fails_time_spec() {
        let value = AnyType::String("2023-10-02T12:00:00Z".to_string());
        let spec = AnyType::String("time".to_string());
        let result = value.matches_type_spec(&spec, "timestamp");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.path, "timestamp");
        assert_eq!(err.expected, "time");
        assert_eq!(err.actual, "string");
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

    #[test]
    fn resolve_parameter_path_simple() {
        let mut params = BTreeMap::new();
        params.insert(
            "device_path".to_string(),
            AnyType::String("string".to_string()),
        );
        assert_eq!(
            resolve_parameter_path(&params, "device_path"),
            Some(&AnyType::String("string".to_string()))
        );
    }

    #[test]
    fn resolve_parameter_path_nested() {
        let mut video = BTreeMap::new();
        video.insert(
            "device_path".to_string(),
            AnyType::String("string".to_string()),
        );
        video.insert("fps".to_string(), AnyType::String("u16".to_string()));
        let mut params = BTreeMap::new();
        params.insert("video".to_string(), AnyType::Object(video));
        assert_eq!(
            resolve_parameter_path(&params, "video.device_path"),
            Some(&AnyType::String("string".to_string()))
        );
        assert_eq!(
            resolve_parameter_path(&params, "video.fps"),
            Some(&AnyType::String("u16".to_string()))
        );
    }

    #[test]
    fn resolve_parameter_path_not_found() {
        let mut params = BTreeMap::new();
        params.insert(
            "device_path".to_string(),
            AnyType::String("string".to_string()),
        );
        assert_eq!(resolve_parameter_path(&params, "nonexistent"), None);
        assert_eq!(resolve_parameter_path(&params, "device_path.nested"), None);
    }

    #[test]
    fn validate_parameter_types_flat_ok() {
        let mut schema = BTreeMap::new();
        schema.insert("fps".to_string(), AnyType::String("u16".to_string()));
        schema.insert("name".to_string(), AnyType::String("string".to_string()));

        let mut args = BTreeMap::new();
        args.insert("fps".to_string(), AnyType::Int(30));
        args.insert("name".to_string(), AnyType::String("cam".to_string()));

        assert!(validate_parameter_types(&args, &schema, "").is_ok());
    }

    #[test]
    fn validate_parameter_types_nested_ok() {
        let mut video_schema = BTreeMap::new();
        video_schema.insert("fps".to_string(), AnyType::String("u16".to_string()));
        let mut schema = BTreeMap::new();
        schema.insert("video".to_string(), AnyType::Object(video_schema));

        let mut video_args = BTreeMap::new();
        video_args.insert("fps".to_string(), AnyType::Int(30));
        let mut args = BTreeMap::new();
        args.insert("video".to_string(), AnyType::Object(video_args));

        assert!(validate_parameter_types(&args, &schema, "").is_ok());
    }

    #[test]
    fn validate_parameter_types_type_mismatch() {
        let mut schema = BTreeMap::new();
        schema.insert("fps".to_string(), AnyType::String("u16".to_string()));

        let mut args = BTreeMap::new();
        args.insert(
            "fps".to_string(),
            AnyType::String("not a number".to_string()),
        );

        let err = validate_parameter_types(&args, &schema, "").unwrap_err();
        assert_eq!(err.path, "fps");
    }

    #[test]
    fn validate_parameter_types_array_schema_expects_array() {
        let mut schema = BTreeMap::new();
        schema.insert(
            "flags".to_string(),
            AnyType::Object(BTreeMap::from([
                ("type".to_string(), AnyType::String("array".to_string())),
                ("items".to_string(), AnyType::String("string".to_string())),
            ])),
        );

        let mut args = BTreeMap::new();
        args.insert("flags".to_string(), AnyType::Int(42));

        let err = validate_parameter_types(&args, &schema, "").unwrap_err();
        assert_eq!(err.path, "flags");
        assert_eq!(err.expected, "array");
    }

    #[test]
    fn validate_parameter_types_object_given_for_array_schema() {
        let mut schema = BTreeMap::new();
        schema.insert(
            "flags".to_string(),
            AnyType::Object(BTreeMap::from([
                ("type".to_string(), AnyType::String("array".to_string())),
                ("items".to_string(), AnyType::String("string".to_string())),
            ])),
        );

        let mut args = BTreeMap::new();
        args.insert(
            "flags".to_string(),
            AnyType::Object(BTreeMap::from([(
                "nested".to_string(),
                AnyType::Bool(true),
            )])),
        );

        let err = validate_parameter_types(&args, &schema, "").unwrap_err();
        assert_eq!(err.path, "flags");
        assert_eq!(err.expected, "array");
        assert_eq!(err.actual, "object");
    }
}

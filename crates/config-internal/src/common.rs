use crate::internal::node::TypeToken;
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

    /// Validates that this runtime value matches a primitive type token.
    ///
    /// Performs strict per-width range checking for integer types (e.g. `u8`
    /// rejects values outside `0..=255`).
    pub fn matches_token(&self, token: &TypeToken, path: &str) -> Result<(), TypeMismatch> {
        let mismatch = |actual_token: &str| TypeMismatch {
            path: path.to_string(),
            expected: type_token_name(token).to_string(),
            actual: actual_token.to_string(),
        };

        match token {
            TypeToken::Bool => match self {
                AnyType::Bool(_) => Ok(()),
                other => Err(mismatch(other.type_name())),
            },
            TypeToken::String => match self {
                AnyType::String(_) => Ok(()),
                other => Err(mismatch(other.type_name())),
            },
            TypeToken::Bytes => match self {
                AnyType::String(_) | AnyType::Array(_) => Ok(()),
                other => Err(mismatch(other.type_name())),
            },
            TypeToken::Time => match self {
                AnyType::Int(_) | AnyType::UInt(_) => Ok(()),
                other => Err(mismatch(other.type_name())),
            },
            TypeToken::U8 => check_unsigned_range(self, 0, u8::MAX as u64, path, token),
            TypeToken::U16 => check_unsigned_range(self, 0, u16::MAX as u64, path, token),
            TypeToken::U32 => check_unsigned_range(self, 0, u32::MAX as u64, path, token),
            TypeToken::U64 => check_unsigned_range(self, 0, u64::MAX, path, token),
            TypeToken::I8 => check_signed_range(self, i8::MIN as i64, i8::MAX as i64, path, token),
            TypeToken::I16 => {
                check_signed_range(self, i16::MIN as i64, i16::MAX as i64, path, token)
            }
            TypeToken::I32 => {
                check_signed_range(self, i32::MIN as i64, i32::MAX as i64, path, token)
            }
            TypeToken::I64 => check_signed_range(self, i64::MIN, i64::MAX, path, token),
            TypeToken::F32 => match check_f32(self) {
                F32Check::Ok(_) => Ok(()),
                F32Check::NotNumeric => Err(mismatch(self.type_name())),
                F32Check::NonFinite => Err(mismatch("non-finite float")),
                F32Check::FloatOutOfRange => Err(mismatch("float outside f32 range")),
                F32Check::IntPrecisionLoss => Err(mismatch(
                    "integer outside f32 lossless range (\u{00b1}2^24)",
                )),
            },
            TypeToken::F64 => match self {
                AnyType::Float(_) | AnyType::Int(_) | AnyType::UInt(_) => Ok(()),
                other => Err(mismatch(other.type_name())),
            },
        }
    }
}

fn check_unsigned_range(
    value: &AnyType,
    min: u64,
    max: u64,
    path: &str,
    token: &TypeToken,
) -> Result<(), TypeMismatch> {
    let v: u64 = match value {
        AnyType::UInt(u) => *u,
        AnyType::Int(i) if *i >= 0 => *i as u64,
        other => {
            return Err(TypeMismatch {
                path: path.to_string(),
                expected: type_token_name(token).to_string(),
                actual: other.type_name().to_string(),
            });
        }
    };
    if v < min || v > max {
        return Err(TypeMismatch {
            path: path.to_string(),
            expected: format!("{} in [{}, {}]", type_token_name(token), min, max),
            actual: format!("{}", v),
        });
    }
    Ok(())
}

fn check_signed_range(
    value: &AnyType,
    min: i64,
    max: i64,
    path: &str,
    token: &TypeToken,
) -> Result<(), TypeMismatch> {
    let v: i64 = match value {
        AnyType::Int(i) => *i,
        AnyType::UInt(u) if *u <= i64::MAX as u64 => *u as i64,
        other => {
            return Err(TypeMismatch {
                path: path.to_string(),
                expected: type_token_name(token).to_string(),
                actual: other.type_name().to_string(),
            });
        }
    };
    if v < min || v > max {
        return Err(TypeMismatch {
            path: path.to_string(),
            expected: format!("{} in [{}, {}]", type_token_name(token), min, max),
            actual: format!("{}", v),
        });
    }
    Ok(())
}

/// Canonical lowercase name for a `TypeToken`. Used in error messages and as
/// the wire-format for shorthand serialization (e.g. `"u16"`).
pub fn type_token_name(token: &TypeToken) -> &'static str {
    match token {
        TypeToken::Bool => "bool",
        TypeToken::String => "string",
        TypeToken::Bytes => "bytes",
        TypeToken::Time => "time",
        TypeToken::U8 => "u8",
        TypeToken::U16 => "u16",
        TypeToken::U32 => "u32",
        TypeToken::U64 => "u64",
        TypeToken::I8 => "i8",
        TypeToken::I16 => "i16",
        TypeToken::I32 => "i32",
        TypeToken::I64 => "i64",
        TypeToken::F32 => "f32",
        TypeToken::F64 => "f64",
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

/// A primitive default value declared in `peppy.json5` via `$default`.
///
/// Type-checked against its `TypeToken` at parse time, so any `DefaultValue`
/// reaching downstream code is guaranteed compatible with its declared kind.
#[derive(Debug, Clone, PartialEq)]
pub enum DefaultValue {
    Bool(bool),
    String(String),
    Int(i64),
    UInt(u64),
    Float(f64),
}

impl DefaultValue {
    /// Materializes this default into the runtime `AnyType` representation
    /// used by `NodeArguments`.
    pub fn to_any(&self) -> AnyType {
        match self {
            DefaultValue::Bool(b) => AnyType::Bool(*b),
            DefaultValue::String(s) => AnyType::String(s.clone()),
            DefaultValue::Int(i) => AnyType::Int(*i),
            DefaultValue::UInt(u) => AnyType::UInt(*u),
            DefaultValue::Float(f) => AnyType::Float(*f),
        }
    }
}

impl Serialize for DefaultValue {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            DefaultValue::Bool(b) => ser.serialize_bool(*b),
            DefaultValue::String(s) => ser.serialize_str(s),
            DefaultValue::Int(i) => ser.serialize_i64(*i),
            DefaultValue::UInt(u) => ser.serialize_u64(*u),
            DefaultValue::Float(f) => ser.serialize_f64(*f),
        }
    }
}

/// A node parameter declaration.
///
/// Parsed at deserialization time:
/// - Shorthand `"u16"` and long form `{ $type: "u16", $default: 30 }` produce [`ParameterSpec::Primitive`].
/// - `{ $type: "array", $items: ..., $length: N? }` produces [`ParameterSpec::Array`].
/// - `{ $type: "object", field: ..., ... }` and a naked object `{ field: ..., ... }`
///   both produce [`ParameterSpec::Group`].
///
/// `$default` is type- and range-checked against `$type` before this value can
/// be constructed, and is permitted only on `Primitive` — never on arrays or
/// groups. This is what makes the schema "parse, don't validate".
#[derive(Debug, Clone, PartialEq)]
pub enum ParameterSpec {
    Primitive {
        kind: TypeToken,
        default: Option<DefaultValue>,
    },
    Array {
        items: Box<ParameterSpec>,
        length: Option<usize>,
    },
    Group(BTreeMap<String, ParameterSpec>),
}

impl ParameterSpec {
    /// Builds the `AnyType` value this spec should default to, recursively
    /// synthesizing `Group`s from their leaves' defaults.
    ///
    /// Arrays cannot carry defaults, so a missing array always reports as
    /// missing. Returns `None` if any reachable leaf lacks a default — in that
    /// case the caller should surface a missing-parameter error naming the
    /// missing path.
    fn synthesize_default(&self, path: &str, missing: &mut Vec<String>) -> Option<AnyType> {
        match self {
            ParameterSpec::Primitive { default, .. } => match default {
                Some(d) => Some(d.to_any()),
                None => {
                    missing.push(path.to_string());
                    None
                }
            },
            ParameterSpec::Array { .. } => {
                missing.push(path.to_string());
                None
            }
            ParameterSpec::Group(map) => {
                let mut out = BTreeMap::new();
                let mut all_present = true;
                for (k, sub) in map {
                    let sub_path = if path.is_empty() {
                        k.clone()
                    } else {
                        format!("{path}.{k}")
                    };
                    match sub.synthesize_default(&sub_path, missing) {
                        Some(v) => {
                            out.insert(k.clone(), v);
                        }
                        None => {
                            all_present = false;
                        }
                    }
                }
                if all_present {
                    Some(AnyType::Object(out))
                } else {
                    None
                }
            }
        }
    }
}

impl Serialize for ParameterSpec {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        match self {
            ParameterSpec::Primitive {
                kind,
                default: None,
            } => ser.serialize_str(type_token_name(kind)),
            ParameterSpec::Primitive {
                kind,
                default: Some(d),
            } => {
                let mut map = ser.serialize_map(Some(2))?;
                map.serialize_entry("$type", type_token_name(kind))?;
                map.serialize_entry("$default", d)?;
                map.end()
            }
            ParameterSpec::Array { items, length } => {
                let len = if length.is_some() { 3 } else { 2 };
                let mut map = ser.serialize_map(Some(len))?;
                map.serialize_entry("$type", "array")?;
                map.serialize_entry("$items", items)?;
                if let Some(l) = length {
                    map.serialize_entry("$length", l)?;
                }
                map.end()
            }
            ParameterSpec::Group(fields) => {
                // Serialize as a naked object (no `$type: "object"` metadata).
                // Both forms parse back to the same `Group` shape.
                let mut map = ser.serialize_map(Some(fields.len()))?;
                for (k, v) in fields {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ParameterSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Two-pass: first deserialize as raw AnyType, then dispatch on shape.
        let raw = AnyType::deserialize(deserializer)?;
        parameter_spec_from_any(raw, "").map_err(serde::de::Error::custom)
    }
}

/// Parse a [`ParameterSpec`] from a raw `AnyType` value.
///
/// `path` is the dot-path prefix used in error messages (empty at the schema root).
fn parameter_spec_from_any(value: AnyType, path: &str) -> Result<ParameterSpec, String> {
    match value {
        AnyType::String(name) => {
            let kind = parse_type_token(&name)
                .ok_or_else(|| format!("at `{path}`: unknown primitive type `{name}`"))?;
            Ok(ParameterSpec::Primitive {
                kind,
                default: None,
            })
        }
        AnyType::Object(map) => {
            // Decide on shape based on `$type`. `$type: "array"` and
            // `$type: "object"` are special; other values are primitive types.
            match map.get("$type") {
                Some(AnyType::String(t)) if t == "array" => parse_array_spec(map, path),
                Some(AnyType::String(t)) if t == "object" => parse_explicit_group(map, path),
                Some(AnyType::String(_)) => parse_primitive_object(map, path),
                Some(other) => Err(format!(
                    "at `{}`: `$type` must be a string, got {}",
                    display_path(path),
                    other.type_name()
                )),
                None => parse_group_object(map, path),
            }
        }
        other => Err(format!(
            "at `{}`: expected primitive type token (string) or parameter group (object), got {}",
            display_path(path),
            other.type_name()
        )),
    }
}

fn parse_primitive_object(
    map: BTreeMap<String, AnyType>,
    path: &str,
) -> Result<ParameterSpec, String> {
    let mut kind: Option<TypeToken> = None;
    let mut default: Option<AnyType> = None;
    for (key, value) in map {
        match key.as_str() {
            "$type" => {
                let AnyType::String(name) = value else {
                    return Err(format!(
                        "at `{}`: `$type` must be a string, got {}",
                        display_path(path),
                        value.type_name()
                    ));
                };
                kind = Some(parse_type_token(&name).ok_or_else(|| {
                    format!(
                        "at `{}`: unknown primitive type `{name}`",
                        display_path(path)
                    )
                })?);
            }
            "$default" => {
                default = Some(value);
            }
            "$optional" => {
                return Err(format!(
                    "at `{}`: `$optional` is not supported on parameters \
                     (it is only for interface message formats; use `$default` for parameters)",
                    display_path(path)
                ));
            }
            other if other.starts_with('$') => {
                return Err(format!(
                    "at `{}`: unknown schema key `{other}`",
                    display_path(path)
                ));
            }
            other => {
                return Err(format!(
                    "at `{}`: parameter object with `$type` cannot also contain regular field `{other}`. \
                     Use a group (object without `$type`) for nested parameters.",
                    display_path(path)
                ));
            }
        }
    }

    let kind = kind.ok_or_else(|| format!("at `{}`: missing `$type`", display_path(path)))?;

    let default = match default {
        Some(raw) => Some(parse_default_value(&kind, raw, path)?),
        None => None,
    };

    Ok(ParameterSpec::Primitive { kind, default })
}

fn parse_array_spec(map: BTreeMap<String, AnyType>, path: &str) -> Result<ParameterSpec, String> {
    let mut items: Option<AnyType> = None;
    let mut length: Option<usize> = None;
    for (key, value) in map {
        match key.as_str() {
            "$type" => { /* already known to be "array" */ }
            "$items" => {
                items = Some(value);
            }
            "$length" => {
                let len = match value {
                    AnyType::UInt(u) => usize::try_from(u).map_err(|_| {
                        format!(
                            "at `{}`: `$length` value {u} does not fit in usize on this target",
                            display_path(path)
                        )
                    })?,
                    AnyType::Int(i) if i >= 0 => usize::try_from(i).map_err(|_| {
                        format!(
                            "at `{}`: `$length` value {i} does not fit in usize on this target",
                            display_path(path)
                        )
                    })?,
                    other => {
                        return Err(format!(
                            "at `{}`: `$length` must be a non-negative integer, got {}",
                            display_path(path),
                            other.type_name()
                        ));
                    }
                };
                length = Some(len);
            }
            "$default" => {
                return Err(format!(
                    "at `{}`: `$default` is not supported on arrays",
                    display_path(path)
                ));
            }
            "$optional" => {
                return Err(format!(
                    "at `{}`: `$optional` is not supported on parameters \
                     (it is only for interface message formats)",
                    display_path(path)
                ));
            }
            other if other.starts_with('$') => {
                return Err(format!(
                    "at `{}`: unknown schema key `{other}` on array",
                    display_path(path)
                ));
            }
            other => {
                return Err(format!(
                    "at `{}`: array spec cannot contain regular field `{other}`",
                    display_path(path)
                ));
            }
        }
    }
    let items = items.ok_or_else(|| {
        format!(
            "at `{}`: array spec is missing `$items`",
            display_path(path)
        )
    })?;
    let item_path = format!("{}[]", if path.is_empty() { "<root>" } else { path });
    let item_spec = parameter_spec_from_any(items, &item_path)?;
    Ok(ParameterSpec::Array {
        items: Box::new(item_spec),
        length,
    })
}

/// Parse `{ $type: "object", field: ..., ... }` into a [`ParameterSpec::Group`].
/// Same shape as a naked group; the explicit `$type: "object"` is just
/// alternate syntax authors may use for clarity.
fn parse_explicit_group(
    map: BTreeMap<String, AnyType>,
    path: &str,
) -> Result<ParameterSpec, String> {
    let mut out = BTreeMap::new();
    for (key, value) in map {
        match key.as_str() {
            "$type" => { /* already known to be "object" */ }
            "$default" => {
                return Err(format!(
                    "at `{}`: `$default` is not supported on object groups",
                    display_path(path)
                ));
            }
            "$optional" => {
                return Err(format!(
                    "at `{}`: `$optional` is not supported on parameters \
                     (it is only for interface message formats)",
                    display_path(path)
                ));
            }
            other if other.starts_with('$') => {
                return Err(format!(
                    "at `{}`: unknown schema key `{other}` on object group",
                    display_path(path)
                ));
            }
            field => {
                let sub_path = if path.is_empty() {
                    field.to_string()
                } else {
                    format!("{path}.{field}")
                };
                let parsed = parameter_spec_from_any(value, &sub_path)?;
                out.insert(field.to_string(), parsed);
            }
        }
    }
    Ok(ParameterSpec::Group(out))
}

fn parse_group_object(map: BTreeMap<String, AnyType>, path: &str) -> Result<ParameterSpec, String> {
    let mut out = BTreeMap::new();
    for (key, value) in map {
        if key.starts_with('$') {
            return Err(format!(
                "at `{}`: schema key `{key}` is not allowed on a parameter group. \
                 `$default` is only valid on primitives; `$optional` is for interfaces, not parameters.",
                display_path(path)
            ));
        }
        let sub_path = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        let parsed = parameter_spec_from_any(value, &sub_path)?;
        out.insert(key, parsed);
    }
    Ok(ParameterSpec::Group(out))
}

fn parse_default_value(kind: &TypeToken, raw: AnyType, path: &str) -> Result<DefaultValue, String> {
    let display = display_path(path);
    match kind {
        TypeToken::Bool => match raw {
            AnyType::Bool(b) => Ok(DefaultValue::Bool(b)),
            other => Err(format!(
                "at `{display}`: `$default` for type `bool` must be a boolean, got {}",
                other.type_name()
            )),
        },
        TypeToken::String => match raw {
            AnyType::String(s) => Ok(DefaultValue::String(s)),
            other => Err(format!(
                "at `{display}`: `$default` for type `string` must be a string, got {}",
                other.type_name()
            )),
        },
        TypeToken::Bytes => Err(format!(
            "at `{display}`: `$default` is not supported for type `bytes`"
        )),
        TypeToken::Time => Err(format!(
            "at `{display}`: `$default` is not supported for type `time`"
        )),
        TypeToken::U8 => bounded_uint_default(raw, 0, u8::MAX as u64, "u8", display),
        TypeToken::U16 => bounded_uint_default(raw, 0, u16::MAX as u64, "u16", display),
        TypeToken::U32 => bounded_uint_default(raw, 0, u32::MAX as u64, "u32", display),
        TypeToken::U64 => bounded_uint_default(raw, 0, u64::MAX, "u64", display),
        TypeToken::I8 => bounded_int_default(raw, i8::MIN as i64, i8::MAX as i64, "i8", display),
        TypeToken::I16 => {
            bounded_int_default(raw, i16::MIN as i64, i16::MAX as i64, "i16", display)
        }
        TypeToken::I32 => {
            bounded_int_default(raw, i32::MIN as i64, i32::MAX as i64, "i32", display)
        }
        TypeToken::I64 => bounded_int_default(raw, i64::MIN, i64::MAX, "i64", display),
        TypeToken::F32 => match check_f32(&raw) {
            F32Check::Ok(f) => Ok(DefaultValue::Float(f)),
            F32Check::NotNumeric => Err(format!(
                "at `{display}`: `$default` for type `f32` must be numeric, got {}",
                raw.type_name()
            )),
            F32Check::NonFinite => Err(format!(
                "at `{display}`: `$default` for type `f32` must be finite"
            )),
            F32Check::FloatOutOfRange => Err(format!(
                "at `{display}`: `$default` value is outside the range of `f32`"
            )),
            F32Check::IntPrecisionLoss => Err(format!(
                "at `{display}`: `$default` integer value exceeds f32 lossless range (\u{00b1}2^24)"
            )),
        },
        TypeToken::F64 => match raw {
            AnyType::Float(f) => Ok(DefaultValue::Float(f)),
            AnyType::Int(i) => Ok(DefaultValue::Float(i as f64)),
            AnyType::UInt(u) => Ok(DefaultValue::Float(u as f64)),
            other => Err(format!(
                "at `{display}`: `$default` for type `f64` must be numeric, got {}",
                other.type_name()
            )),
        },
    }
}

fn bounded_uint_default(
    raw: AnyType,
    min: u64,
    max: u64,
    name: &str,
    display: &str,
) -> Result<DefaultValue, String> {
    let v: u64 = match raw {
        AnyType::UInt(u) => u,
        AnyType::Int(i) if i >= 0 => i as u64,
        AnyType::Int(i) => {
            return Err(format!(
                "at `{display}`: `$default` for unsigned type `{name}` must be non-negative, got {i}"
            ));
        }
        other => {
            return Err(format!(
                "at `{display}`: `$default` for type `{name}` must be an integer, got {}",
                other.type_name()
            ));
        }
    };
    if v < min || v > max {
        return Err(format!(
            "at `{display}`: `$default` value {v} is outside the range of `{name}` [{min}, {max}]"
        ));
    }
    Ok(DefaultValue::UInt(v))
}

fn bounded_int_default(
    raw: AnyType,
    min: i64,
    max: i64,
    name: &str,
    display: &str,
) -> Result<DefaultValue, String> {
    let v: i64 = match raw {
        AnyType::Int(i) => i,
        AnyType::UInt(u) if u <= i64::MAX as u64 => u as i64,
        AnyType::UInt(u) => {
            return Err(format!(
                "at `{display}`: `$default` value {u} is outside the range of `{name}` [{min}, {max}]"
            ));
        }
        other => {
            return Err(format!(
                "at `{display}`: `$default` for type `{name}` must be an integer, got {}",
                other.type_name()
            ));
        }
    };
    if v < min || v > max {
        return Err(format!(
            "at `{display}`: `$default` value {v} is outside the range of `{name}` [{min}, {max}]"
        ));
    }
    Ok(DefaultValue::Int(v))
}

/// Largest absolute integer that round-trips through f32 without precision
/// loss. f32 has a 24-bit mantissa, so values in `±2^24` are representable
/// exactly; `2^24 + 1` is not.
const F32_LOSSLESS_INT_LIMIT: u64 = 1 << f32::MANTISSA_DIGITS;

/// Outcome of checking whether a numeric `AnyType` can be safely stored as
/// f32. Variants distinguish failure modes so each caller can format an error
/// in its own idiom while sharing the underlying range logic.
enum F32Check {
    Ok(f64),
    NotNumeric,
    NonFinite,
    FloatOutOfRange,
    IntPrecisionLoss,
}

fn check_f32(value: &AnyType) -> F32Check {
    match value {
        AnyType::Float(f) if !f.is_finite() => F32Check::NonFinite,
        AnyType::Float(f) if f.abs() > f32::MAX as f64 => F32Check::FloatOutOfRange,
        AnyType::Float(f) => F32Check::Ok(*f),
        AnyType::Int(i) if i.unsigned_abs() > F32_LOSSLESS_INT_LIMIT => F32Check::IntPrecisionLoss,
        AnyType::Int(i) => F32Check::Ok(*i as f64),
        AnyType::UInt(u) if *u > F32_LOSSLESS_INT_LIMIT => F32Check::IntPrecisionLoss,
        AnyType::UInt(u) => F32Check::Ok(*u as f64),
        _ => F32Check::NotNumeric,
    }
}

/// Parse a primitive type-token name (with aliases). Returns `None` for unknown
/// names so callers can build their own error messages with context.
fn parse_type_token(name: &str) -> Option<TypeToken> {
    Some(match name {
        "bool" => TypeToken::Bool,
        "string" | "str" => TypeToken::String,
        "bytes" => TypeToken::Bytes,
        "time" => TypeToken::Time,
        "u8" => TypeToken::U8,
        "u16" => TypeToken::U16,
        "u32" => TypeToken::U32,
        "u64" => TypeToken::U64,
        "i8" => TypeToken::I8,
        "i16" => TypeToken::I16,
        "i32" => TypeToken::I32,
        "i64" => TypeToken::I64,
        "f32" | "float" => TypeToken::F32,
        "f64" | "double" => TypeToken::F64,
        _ => return None,
    })
}

fn display_path(path: &str) -> &str {
    if path.is_empty() { "<root>" } else { path }
}

/// Type alias for parameter type specifications declared in `peppy.json5`.
///
/// Each key is a parameter name and each value is a typed [`ParameterSpec`]
/// — either a primitive leaf (with optional `$default`) or a nested group.
pub type ParameterSchema = BTreeMap<String, ParameterSpec>;

/// Unvalidated node arguments deserialized from a runtime or deployment config.
///
/// Use [`RawNodeArguments::into_resolved`] to validate against a
/// [`ParameterSchema`] and obtain a [`NodeArguments`] value.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub(crate) struct RawNodeArguments(BTreeMap<String, AnyType>);

impl RawNodeArguments {
    /// Validate these raw arguments against a [`ParameterSchema`] and produce
    /// a [`NodeArguments`] value.
    ///
    /// Behavior:
    /// 1. For every schema key missing from the arguments, synthesize from
    ///    `$default` if the entire subtree has defaults; otherwise collect a
    ///    missing-parameter error naming the leaf path that has no default.
    ///    Partially supplied groups have their missing children synthesized
    ///    from `$default` recursively.
    /// 2. Validate every supplied value against its declared type, with strict
    ///    per-width range checks for integer types.
    /// 3. Reject any argument key not declared in the schema.
    fn into_resolved(
        mut self,
        schema: &ParameterSchema,
    ) -> Result<NodeArguments, NodeArgumentsError> {
        // 1. Fill defaults for any keys missing from the supplied arguments,
        //    recursing into supplied groups to fill their missing children too.
        let mut missing: Vec<String> = Vec::new();
        for (key, spec) in schema {
            match self.0.get_mut(key) {
                Some(value) => merge_defaults(value, spec, key, &mut missing),
                None => {
                    if let Some(default_value) = spec.synthesize_default(key, &mut missing) {
                        self.0.insert(key.clone(), default_value);
                    }
                }
            }
        }
        if !missing.is_empty() {
            return Err(NodeArgumentsError::MissingParameters(missing));
        }

        // 2. Validate every supplied value against its declared spec.
        for (key, value) in &self.0 {
            let spec = schema
                .get(key)
                .ok_or_else(|| NodeArgumentsError::UnknownParameter { key: key.clone() })?;
            validate_value_against_spec(value, spec, key)
                .map_err(NodeArgumentsError::TypeMismatch)?;
        }

        Ok(NodeArguments(self.0))
    }
}

/// Walk an existing actual value alongside its spec, filling any missing
/// children of group-typed values with their `$default` synthesizations.
///
/// This is the merge counterpart to [`ParameterSpec::synthesize_default`]:
/// that one builds a value from defaults alone for a fully missing subtree;
/// this one fills the gaps inside a partially supplied subtree. Required
/// leaves (no `$default`) that remain missing are appended to `missing` as
/// dot-paths so the caller can surface a [`NodeArgumentsError::MissingParameters`].
///
/// No-ops when the spec/value pair isn't `Group`/`Object` or `Array`/`Array`;
/// type-mismatch reporting is left to [`validate_value_against_spec`].
fn merge_defaults(
    value: &mut AnyType,
    spec: &ParameterSpec,
    path: &str,
    missing: &mut Vec<String>,
) {
    match (spec, value) {
        (ParameterSpec::Group(fields), AnyType::Object(map)) => {
            for (key, sub_spec) in fields {
                let sub_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                match map.get_mut(key) {
                    Some(child) => merge_defaults(child, sub_spec, &sub_path, missing),
                    None => {
                        if let Some(synthesized) = sub_spec.synthesize_default(&sub_path, missing) {
                            map.insert(key.clone(), synthesized);
                        }
                    }
                }
            }
        }
        (ParameterSpec::Array { items, .. }, AnyType::Array(arr)) => {
            for (i, elem) in arr.iter_mut().enumerate() {
                let sub_path = format!("{path}[{i}]");
                merge_defaults(elem, items, &sub_path, missing);
            }
        }
        _ => {}
    }
}

/// Recursively validates a runtime value against a typed [`ParameterSpec`].
fn validate_value_against_spec(
    value: &AnyType,
    spec: &ParameterSpec,
    path: &str,
) -> Result<(), TypeMismatch> {
    match spec {
        ParameterSpec::Primitive { kind, .. } => value.matches_token(kind, path),
        ParameterSpec::Array { items, length } => {
            let AnyType::Array(arr) = value else {
                return Err(TypeMismatch {
                    path: path.to_string(),
                    expected: "array".to_string(),
                    actual: value.type_name().to_string(),
                });
            };
            if let Some(expected_len) = length
                && arr.len() != *expected_len
            {
                return Err(TypeMismatch {
                    path: path.to_string(),
                    expected: format!("array of length {expected_len}"),
                    actual: format!("array of length {}", arr.len()),
                });
            }
            for (i, elem) in arr.iter().enumerate() {
                let item_path = format!("{path}[{i}]");
                validate_value_against_spec(elem, items, &item_path)?;
            }
            Ok(())
        }
        ParameterSpec::Group(fields) => {
            let AnyType::Object(map) = value else {
                return Err(TypeMismatch {
                    path: path.to_string(),
                    expected: "object".to_string(),
                    actual: value.type_name().to_string(),
                });
            };
            // Reject unknown keys.
            for k in map.keys() {
                if !fields.contains_key(k) {
                    return Err(TypeMismatch {
                        path: format!("{path}.{k}"),
                        expected: "field defined in schema".to_string(),
                        actual: "unknown field".to_string(),
                    });
                }
            }
            // Validate each declared field — defaults have already been filled
            // in by `into_resolved`, so any missing key here is a true error.
            for (k, sub_spec) in fields {
                let sub_path = format!("{path}.{k}");
                match map.get(k) {
                    Some(v) => validate_value_against_spec(v, sub_spec, &sub_path)?,
                    None => {
                        return Err(TypeMismatch {
                            path: sub_path,
                            expected: "value".to_string(),
                            actual: "missing".to_string(),
                        });
                    }
                }
            }
            Ok(())
        }
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

/// Error produced when node argument parsing or validation fails.
#[derive(Debug)]
pub enum NodeArgumentsError {
    /// One or more runtime argument values do not match the schema types.
    TypeMismatch(TypeMismatch),
    /// One or more parameters declared in the schema are missing from the
    /// arguments and have no `$default` to fall back on. Each entry is a full
    /// dot-path (e.g. `device.serial`).
    MissingParameters(Vec<String>),
    /// An argument key is not declared in the schema.
    UnknownParameter { key: String },
    /// The input string could not be deserialized.
    Deserialization(String),
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
            Self::Deserialization(msg) => {
                write!(f, "failed to deserialize arguments: {msg}")
            }
        }
    }
}

impl std::error::Error for NodeArgumentsError {}

/// Validates a `BTreeMap<String, AnyType>` against a [`ParameterSchema`] and
/// produces a [`NodeArguments`] value.
///
/// Use this when arguments are constructed programmatically (e.g. from a
/// deserialized runtime config) rather than parsed from a raw string.
pub fn validate_node_arguments(
    arguments: BTreeMap<String, AnyType>,
    schema: &ParameterSchema,
) -> Result<NodeArguments, NodeArgumentsError> {
    let raw = RawNodeArguments::from(arguments);
    raw.into_resolved(schema)
}

/// Fill any keys missing from `arguments` with values synthesized from the
/// schema's `$default` fallbacks, without validating types or unknown keys.
///
/// Returned [`Vec`] is the dot-paths of leaves required by the schema that
/// have no `$default` to synthesize from. The argument map is left unchanged
/// when the result is non-empty.
///
/// Use this at the daemon edge before spawning a node, when the spawned node
/// is responsible for its own full validation. For one-shot validation +
/// defaulting in a single call, use [`validate_node_arguments`] instead.
pub fn apply_parameter_defaults(
    arguments: &mut BTreeMap<String, AnyType>,
    schema: &ParameterSchema,
) -> Vec<String> {
    // Stage mutations on a clone so partially synthesized values are not
    // visible to the caller when we end up returning a non-empty `missing`.
    let mut staged = arguments.clone();
    let mut missing = Vec::new();
    for (key, spec) in schema {
        match staged.get_mut(key) {
            Some(value) => merge_defaults(value, spec, key, &mut missing),
            None => {
                if let Some(value) = spec.synthesize_default(key, &mut missing) {
                    staged.insert(key.clone(), value);
                }
            }
        }
    }
    if missing.is_empty() {
        *arguments = staged;
    }
    missing
}

/// Resolve a dot-path (e.g., `"video.device_path"`) against a parameter schema,
/// returning the leaf [`ParameterSpec`] if found.
///
/// Descends into [`ParameterSpec::Group`] values at each segment boundary.
pub fn resolve_parameter_path<'a>(
    parameters: &'a ParameterSchema,
    dot_path: &str,
) -> Option<&'a ParameterSpec> {
    let mut segments = dot_path.split('.');
    let first = segments.next()?;
    let mut current = parameters.get(first)?;

    for segment in segments {
        match current {
            ParameterSpec::Group(map) => {
                current = map.get(segment)?;
            }
            _ => return None,
        }
    }

    Some(current)
}

/// Resolve a dot-path against a tree of runtime parameter VALUES (a
/// `BTreeMap<String, AnyType>`), descending into [`AnyType::Object`] groups.
///
/// This is the value-side counterpart to `resolve_parameter_path`: that one
/// walks the schema and returns a [`ParameterSpec`]; this one walks resolved
/// arguments and returns the concrete [`AnyType`] value at the leaf.
pub fn resolve_argument_path<'a>(
    arguments: &'a BTreeMap<String, AnyType>,
    dot_path: &str,
) -> Option<&'a AnyType> {
    let mut segments = dot_path.split('.');
    let first = segments.next()?;
    let mut current = arguments.get(first)?;

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

    fn json5<T: serde::de::DeserializeOwned>(s: &str) -> T {
        serde_json5::from_str(s).unwrap()
    }

    fn schema(s: &str) -> ParameterSchema {
        json5(s)
    }

    // ---- Shorthand and long-form parsing ----

    #[test]
    fn shorthand_parses_to_primitive() {
        let parsed: ParameterSpec = json5(r#""u16""#);
        assert_eq!(
            parsed,
            ParameterSpec::Primitive {
                kind: TypeToken::U16,
                default: None
            }
        );
    }

    #[test]
    fn shorthand_alias_str_parses_as_string() {
        let parsed: ParameterSpec = json5(r#""str""#);
        assert!(matches!(
            parsed,
            ParameterSpec::Primitive {
                kind: TypeToken::String,
                default: None
            }
        ));
    }

    #[test]
    fn long_form_with_default_parses() {
        let parsed: ParameterSpec = json5(r#"{ $type: "u16", $default: 30 }"#);
        assert_eq!(
            parsed,
            ParameterSpec::Primitive {
                kind: TypeToken::U16,
                default: Some(DefaultValue::UInt(30))
            }
        );
    }

    #[test]
    fn long_form_string_default_parses() {
        let parsed: ParameterSpec = json5(r#"{ $type: "string", $default: "/dev/video0" }"#);
        assert_eq!(
            parsed,
            ParameterSpec::Primitive {
                kind: TypeToken::String,
                default: Some(DefaultValue::String("/dev/video0".into()))
            }
        );
    }

    #[test]
    fn long_form_bool_default_parses() {
        let parsed: ParameterSpec = json5(r#"{ $type: "bool", $default: true }"#);
        assert_eq!(
            parsed,
            ParameterSpec::Primitive {
                kind: TypeToken::Bool,
                default: Some(DefaultValue::Bool(true))
            }
        );
    }

    #[test]
    fn long_form_signed_negative_default_parses() {
        let parsed: ParameterSpec = json5(r#"{ $type: "i32", $default: -7 }"#);
        assert_eq!(
            parsed,
            ParameterSpec::Primitive {
                kind: TypeToken::I32,
                default: Some(DefaultValue::Int(-7))
            }
        );
    }

    #[test]
    fn group_parses_recursively() {
        let parsed: ParameterSchema = schema(
            r#"{
                video: {
                    fps: "u16",
                    resolution: { width: "u16", height: "u16" }
                }
            }"#,
        );
        let video = parsed.get("video").unwrap();
        let ParameterSpec::Group(video_fields) = video else {
            panic!("expected group");
        };
        assert!(matches!(
            video_fields.get("fps"),
            Some(ParameterSpec::Primitive {
                kind: TypeToken::U16,
                default: None
            })
        ));
        let resolution = video_fields.get("resolution").unwrap();
        let ParameterSpec::Group(_) = resolution else {
            panic!("expected nested group");
        };
    }

    #[test]
    fn mixed_shorthand_and_long_form_in_same_group() {
        let parsed: ParameterSchema = schema(
            r#"{
                device: {
                    path: { $type: "string", $default: "/dev/video0" },
                    serial: "string"
                }
            }"#,
        );
        let ParameterSpec::Group(fields) = parsed.get("device").unwrap() else {
            panic!("expected group");
        };
        assert!(matches!(
            fields.get("path"),
            Some(ParameterSpec::Primitive {
                kind: TypeToken::String,
                default: Some(DefaultValue::String(_))
            })
        ));
        assert!(matches!(
            fields.get("serial"),
            Some(ParameterSpec::Primitive {
                kind: TypeToken::String,
                default: None
            })
        ));
    }

    // ---- Parse-time validation errors ----

    #[test]
    fn default_type_mismatch_at_parse_time() {
        let result: Result<ParameterSpec, _> =
            serde_json5::from_str(r#"{ $type: "u16", $default: "thirty" }"#);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("$default"), "got: {err}");
        assert!(err.contains("u16"), "got: {err}");
    }

    #[test]
    fn default_out_of_range_u8() {
        let result: Result<ParameterSpec, _> =
            serde_json5::from_str(r#"{ $type: "u8", $default: 300 }"#);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("u8"), "got: {err}");
        assert!(err.contains("300"), "got: {err}");
    }

    #[test]
    fn default_out_of_range_i16() {
        let result: Result<ParameterSpec, _> =
            serde_json5::from_str(r#"{ $type: "i16", $default: 70000 }"#);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("i16"), "got: {err}");
    }

    #[test]
    fn default_negative_for_unsigned() {
        let result: Result<ParameterSpec, _> =
            serde_json5::from_str(r#"{ $type: "u32", $default: -1 }"#);
        assert!(result.is_err());
    }

    #[test]
    fn default_on_group_rejected() {
        let result: Result<ParameterSpec, _> =
            serde_json5::from_str(r#"{ width: "u16", $default: 5 }"#);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("$default"), "got: {err}");
    }

    #[test]
    fn optional_on_parameter_rejected() {
        let result: Result<ParameterSpec, _> =
            serde_json5::from_str(r#"{ $type: "string", $optional: true }"#);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("$optional"), "got: {err}");
        assert!(err.contains("$default"), "got: {err}");
    }

    #[test]
    fn unknown_dollar_key_rejected() {
        let result: Result<ParameterSpec, _> =
            serde_json5::from_str(r#"{ $type: "string", $frobnicate: 1 }"#);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("$frobnicate"), "got: {err}");
    }

    #[test]
    fn unknown_type_rejected() {
        let result: Result<ParameterSpec, _> = serde_json5::from_str(r#""qubit""#);
        assert!(result.is_err());
    }

    #[test]
    fn type_object_with_extra_field_rejected() {
        let result: Result<ParameterSpec, _> =
            serde_json5::from_str(r#"{ $type: "u16", whatever: 1 }"#);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("whatever"), "got: {err}");
    }

    #[test]
    fn default_on_bytes_rejected() {
        let result: Result<ParameterSpec, _> =
            serde_json5::from_str(r#"{ $type: "bytes", $default: "abc" }"#);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("bytes"), "got: {err}");
    }

    #[test]
    fn default_on_time_rejected() {
        let result: Result<ParameterSpec, _> =
            serde_json5::from_str(r#"{ $type: "time", $default: 1 }"#);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("time"), "got: {err}");
    }

    #[test]
    fn float_default_accepts_int_literal() {
        let parsed: ParameterSpec = json5(r#"{ $type: "f32", $default: 1 }"#);
        assert_eq!(
            parsed,
            ParameterSpec::Primitive {
                kind: TypeToken::F32,
                default: Some(DefaultValue::Float(1.0))
            }
        );
    }

    // ---- F32 lossless representation checks ----

    #[test]
    fn f32_default_accepts_max_lossless_int() {
        let parsed: ParameterSpec = json5(r#"{ $type: "f32", $default: 16777216 }"#);
        assert_eq!(
            parsed,
            ParameterSpec::Primitive {
                kind: TypeToken::F32,
                default: Some(DefaultValue::Float(16777216.0))
            }
        );
    }

    #[test]
    fn f32_default_rejects_int_above_lossless_range() {
        let result: Result<ParameterSpec, _> =
            serde_json5::from_str(r#"{ $type: "f32", $default: 16777217 }"#);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("f32"), "got: {err}");
        assert!(err.contains("lossless"), "got: {err}");
    }

    #[test]
    fn f32_default_rejects_negative_int_below_lossless_range() {
        let result: Result<ParameterSpec, _> =
            serde_json5::from_str(r#"{ $type: "f32", $default: -16777217 }"#);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("f32"), "got: {err}");
    }

    #[test]
    fn f32_default_rejects_float_overflow() {
        let result: Result<ParameterSpec, _> =
            serde_json5::from_str(r#"{ $type: "f32", $default: 1e100 }"#);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("f32"), "got: {err}");
        assert!(err.contains("range"), "got: {err}");
    }

    #[test]
    fn f64_default_accepts_int_above_f32_range() {
        let parsed: ParameterSpec = json5(r#"{ $type: "f64", $default: 16777217 }"#);
        assert!(matches!(
            parsed,
            ParameterSpec::Primitive {
                kind: TypeToken::F64,
                default: Some(DefaultValue::Float(_))
            }
        ));
    }

    #[test]
    fn f64_default_accepts_large_float() {
        let parsed: ParameterSpec = json5(r#"{ $type: "f64", $default: 1e100 }"#);
        assert!(matches!(
            parsed,
            ParameterSpec::Primitive {
                kind: TypeToken::F64,
                default: Some(DefaultValue::Float(_))
            }
        ));
    }

    #[test]
    fn runtime_f32_rejects_int_above_lossless_range() {
        let s = schema(r#"{ gain: "f32" }"#);
        let raw = RawNodeArguments::from([("gain".to_string(), AnyType::Int(16777217))]);
        let err = raw.into_resolved(&s).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("f32"), "got: {msg}");
    }

    #[test]
    fn runtime_f32_rejects_float_overflow() {
        let s = schema(r#"{ gain: "f32" }"#);
        let raw = RawNodeArguments::from([("gain".to_string(), AnyType::Float(1e100))]);
        let err = raw.into_resolved(&s).unwrap_err();
        assert!(matches!(err, NodeArgumentsError::TypeMismatch(_)));
    }

    #[test]
    fn runtime_f32_rejects_non_finite_float() {
        let s = schema(r#"{ gain: "f32" }"#);
        let raw = RawNodeArguments::from([("gain".to_string(), AnyType::Float(f64::INFINITY))]);
        let err = raw.into_resolved(&s).unwrap_err();
        assert!(matches!(err, NodeArgumentsError::TypeMismatch(_)));

        let raw_nan = RawNodeArguments::from([("gain".to_string(), AnyType::Float(f64::NAN))]);
        assert!(raw_nan.into_resolved(&s).is_err());
    }

    #[test]
    fn runtime_f32_accepts_lossless_values() {
        let s = schema(r#"{ gain: "f32" }"#);
        for value in [
            AnyType::Float(0.0),
            AnyType::Float(1.5),
            AnyType::Float(-1.5),
            AnyType::Int(16777216),
            AnyType::Int(-16777216),
            AnyType::UInt(16777216),
            AnyType::UInt(0),
        ] {
            let raw = RawNodeArguments::from([("gain".to_string(), value.clone())]);
            assert!(
                raw.into_resolved(&s).is_ok(),
                "expected {value:?} to validate against f32"
            );
        }
    }

    #[test]
    fn runtime_f64_accepts_value_above_f32_range() {
        let s = schema(r#"{ gain: "f64" }"#);
        let raw = RawNodeArguments::from([("gain".to_string(), AnyType::Float(1e100))]);
        assert!(raw.into_resolved(&s).is_ok());
    }

    // ---- Array parameter parsing ----

    #[test]
    fn array_with_primitive_items_parses() {
        let parsed: ParameterSpec = json5(r#"{ $type: "array", $items: "string" }"#);
        match parsed {
            ParameterSpec::Array { items, length } => {
                assert!(matches!(
                    *items,
                    ParameterSpec::Primitive {
                        kind: TypeToken::String,
                        ..
                    }
                ));
                assert_eq!(length, None);
            }
            other => panic!("expected array, got: {other:?}"),
        }
    }

    #[test]
    fn array_with_length_parses() {
        let parsed: ParameterSpec = json5(r#"{ $type: "array", $items: "f32", $length: 3 }"#);
        match parsed {
            ParameterSpec::Array { length, .. } => assert_eq!(length, Some(3)),
            other => panic!("expected array, got: {other:?}"),
        }
    }

    #[test]
    fn array_default_rejected() {
        let result: Result<ParameterSpec, _> =
            serde_json5::from_str(r#"{ $type: "array", $items: "string", $default: ["a"] }"#);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("$default"), "got: {err}");
        assert!(err.contains("array"), "got: {err}");
    }

    #[test]
    fn explicit_object_type_parses_as_group() {
        let parsed: ParameterSpec = json5(r#"{ $type: "object", enabled: "bool", gain: "i64" }"#);
        let ParameterSpec::Group(fields) = parsed else {
            panic!("expected group");
        };
        assert!(matches!(
            fields.get("enabled"),
            Some(ParameterSpec::Primitive {
                kind: TypeToken::Bool,
                ..
            })
        ));
        assert!(matches!(
            fields.get("gain"),
            Some(ParameterSpec::Primitive {
                kind: TypeToken::I64,
                ..
            })
        ));
    }

    #[test]
    fn explicit_object_with_default_rejected() {
        let result: Result<ParameterSpec, _> =
            serde_json5::from_str(r#"{ $type: "object", x: "u8", $default: {} }"#);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("$default"), "got: {err}");
        assert!(err.contains("object"), "got: {err}");
    }

    #[test]
    fn into_resolved_validates_array_items() {
        let s = schema(r#"{ tags: { $type: "array", $items: "string" } }"#);
        let raw = RawNodeArguments::from([(
            "tags".to_string(),
            AnyType::Array(vec![AnyType::String("a".into()), AnyType::Int(1)]),
        )]);
        let err = raw.into_resolved(&s).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("tags[1]"), "got: {msg}");
    }

    // ---- Serialization round-trip ----

    #[test]
    fn shorthand_round_trips_to_bare_string() {
        let spec = ParameterSpec::Primitive {
            kind: TypeToken::U16,
            default: None,
        };
        let serialized = serde_json5::to_string(&spec).unwrap();
        assert_eq!(serialized, r#""u16""#);
    }

    #[test]
    fn long_form_round_trips() {
        let spec = ParameterSpec::Primitive {
            kind: TypeToken::String,
            default: Some(DefaultValue::String("hello".into())),
        };
        let serialized = serde_json5::to_string(&spec).unwrap();
        let reparsed: ParameterSpec = serde_json5::from_str(&serialized).unwrap();
        assert_eq!(spec, reparsed);
    }

    // ---- Runtime defaulting via into_resolved ----

    #[test]
    fn missing_leaf_with_default_is_synthesized() {
        let s = schema(r#"{ name: { $type: "string", $default: "world" } }"#);
        let raw = RawNodeArguments::from(BTreeMap::new());
        let args = raw.into_resolved(&s).unwrap();
        assert_eq!(
            args.0.get("name"),
            Some(&AnyType::String("world".to_string()))
        );
    }

    #[test]
    fn missing_leaf_without_default_errors() {
        let s = schema(r#"{ name: "string" }"#);
        let raw = RawNodeArguments::from(BTreeMap::new());
        let err = raw.into_resolved(&s).unwrap_err();
        let NodeArgumentsError::MissingParameters(keys) = err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(keys, vec!["name".to_string()]);
    }

    #[test]
    fn user_value_overrides_default() {
        let s = schema(r#"{ name: { $type: "string", $default: "world" } }"#);
        let raw =
            RawNodeArguments::from([("name".to_string(), AnyType::String("override".to_string()))]);
        let args = raw.into_resolved(&s).unwrap();
        assert_eq!(
            args.0.get("name"),
            Some(&AnyType::String("override".to_string()))
        );
    }

    #[test]
    fn missing_group_synthesized_when_all_leaves_have_defaults() {
        let s = schema(
            r#"{
                device: {
                    path: { $type: "string", $default: "/dev/video0" },
                    auto_detect: { $type: "bool", $default: true }
                }
            }"#,
        );
        let raw = RawNodeArguments::from(BTreeMap::new());
        let args = raw.into_resolved(&s).unwrap();
        let AnyType::Object(device) = args.0.get("device").unwrap() else {
            panic!("expected synthesized object");
        };
        assert_eq!(
            device.get("path"),
            Some(&AnyType::String("/dev/video0".into()))
        );
        assert_eq!(device.get("auto_detect"), Some(&AnyType::Bool(true)));
    }

    #[test]
    fn partial_group_synthesizes_missing_child_with_default() {
        // User supplies `device: {}` with the `path` child missing; the
        // schema's `$default` should still be filled in.
        let s = schema(
            r#"{
                device: {
                    path: { $type: "string", $default: "/dev/video0" }
                }
            }"#,
        );
        let raw =
            RawNodeArguments::from([("device".to_string(), AnyType::Object(BTreeMap::new()))]);
        let args = raw.into_resolved(&s).unwrap();
        let AnyType::Object(device) = args.0.get("device").unwrap() else {
            panic!("expected device object");
        };
        assert_eq!(
            device.get("path"),
            Some(&AnyType::String("/dev/video0".into()))
        );
    }

    #[test]
    fn partial_group_preserves_user_supplied_child_value() {
        let s = schema(
            r#"{
                device: {
                    path: { $type: "string", $default: "/dev/video0" },
                    auto_detect: { $type: "bool", $default: true }
                }
            }"#,
        );
        let raw = RawNodeArguments::from([(
            "device".to_string(),
            AnyType::Object(BTreeMap::from([(
                "path".to_string(),
                AnyType::String("/dev/video1".into()),
            )])),
        )]);
        let args = raw.into_resolved(&s).unwrap();
        let AnyType::Object(device) = args.0.get("device").unwrap() else {
            panic!("expected device object");
        };
        assert_eq!(
            device.get("path"),
            Some(&AnyType::String("/dev/video1".into()))
        );
        assert_eq!(device.get("auto_detect"), Some(&AnyType::Bool(true)));
    }

    #[test]
    fn partial_group_reports_missing_required_child() {
        // User supplies an empty group; the required (no-default) child must
        // be reported with its dot-path. `serial` is a USB serial number that
        // varies per unit and has no sensible default.
        let s = schema(
            r#"{
                device: {
                    path: { $type: "string", $default: "/dev/video0" },
                    serial: "string"
                }
            }"#,
        );
        let raw =
            RawNodeArguments::from([("device".to_string(), AnyType::Object(BTreeMap::new()))]);
        let err = raw.into_resolved(&s).unwrap_err();
        let NodeArgumentsError::MissingParameters(keys) = err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(keys, vec!["device.serial".to_string()]);
    }

    #[test]
    fn nested_partial_groups_are_merged_recursively() {
        let s = schema(
            r#"{
                video: {
                    resolution: {
                        width: { $type: "u16", $default: 1920 },
                        height: { $type: "u16", $default: 1080 }
                    }
                }
            }"#,
        );
        let raw = RawNodeArguments::from([(
            "video".to_string(),
            AnyType::Object(BTreeMap::from([(
                "resolution".to_string(),
                AnyType::Object(BTreeMap::from([("width".to_string(), AnyType::UInt(640))])),
            )])),
        )]);
        let args = raw.into_resolved(&s).unwrap();
        let AnyType::Object(video) = args.0.get("video").unwrap() else {
            panic!("expected video object");
        };
        let AnyType::Object(resolution) = video.get("resolution").unwrap() else {
            panic!("expected resolution object");
        };
        assert_eq!(resolution.get("width"), Some(&AnyType::UInt(640)));
        assert_eq!(resolution.get("height"), Some(&AnyType::UInt(1080)));
    }

    #[test]
    fn apply_parameter_defaults_fills_partial_group() {
        let s = schema(
            r#"{
                device: {
                    path: { $type: "string", $default: "/dev/video0" }
                }
            }"#,
        );
        let mut args: BTreeMap<String, AnyType> =
            BTreeMap::from([("device".to_string(), AnyType::Object(BTreeMap::new()))]);
        let missing = apply_parameter_defaults(&mut args, &s);
        assert!(missing.is_empty(), "got missing: {missing:?}");
        let AnyType::Object(device) = args.get("device").unwrap() else {
            panic!("expected device object");
        };
        assert_eq!(
            device.get("path"),
            Some(&AnyType::String("/dev/video0".into()))
        );
    }

    #[test]
    fn apply_parameter_defaults_leaves_arguments_untouched_on_missing() {
        let s = schema(
            r#"{
                device: {
                    path: { $type: "string", $default: "/dev/video0" },
                    serial: "string"
                }
            }"#,
        );
        let original_device = AnyType::Object(BTreeMap::new());
        let mut args: BTreeMap<String, AnyType> =
            BTreeMap::from([("device".to_string(), original_device.clone())]);
        let missing = apply_parameter_defaults(&mut args, &s);
        assert_eq!(missing, vec!["device.serial".to_string()]);
        assert_eq!(args.get("device"), Some(&original_device));
    }

    #[test]
    fn missing_group_with_partial_defaults_reports_required_leaf() {
        let s = schema(
            r#"{
                device: {
                    path: { $type: "string", $default: "/dev/video0" },
                    serial: "string"
                }
            }"#,
        );
        let raw = RawNodeArguments::from(BTreeMap::new());
        let err = raw.into_resolved(&s).unwrap_err();
        let NodeArgumentsError::MissingParameters(keys) = err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(keys, vec!["device.serial".to_string()]);
    }

    #[test]
    fn user_supplied_value_type_mismatch_errors() {
        let s = schema(r#"{ fps: "u16" }"#);
        let raw =
            RawNodeArguments::from([("fps".to_string(), AnyType::String("not a number".into()))]);
        let err = raw.into_resolved(&s).unwrap_err();
        assert!(matches!(err, NodeArgumentsError::TypeMismatch(_)));
    }

    #[test]
    fn user_supplied_uint_out_of_range_errors() {
        let s = schema(r#"{ count: "u8" }"#);
        let raw = RawNodeArguments::from([("count".to_string(), AnyType::Int(300))]);
        let err = raw.into_resolved(&s).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("u8"), "got: {msg}");
    }

    #[test]
    fn user_supplied_negative_for_unsigned_errors() {
        let s = schema(r#"{ count: "u32" }"#);
        let raw = RawNodeArguments::from([("count".to_string(), AnyType::Int(-1))]);
        let err = raw.into_resolved(&s).unwrap_err();
        assert!(matches!(err, NodeArgumentsError::TypeMismatch(_)));
    }

    #[test]
    fn unknown_argument_key_errors() {
        let s = schema(r#"{ name: "string" }"#);
        let raw = RawNodeArguments::from([
            ("name".to_string(), AnyType::String("a".into())),
            ("extra".to_string(), AnyType::String("b".into())),
        ]);
        let err = raw.into_resolved(&s).unwrap_err();
        assert!(matches!(err, NodeArgumentsError::UnknownParameter { .. }));
    }

    // ---- resolve_parameter_path ----

    #[test]
    fn resolve_parameter_path_simple() {
        let params = schema(r#"{ device_path: "string" }"#);
        let resolved = resolve_parameter_path(&params, "device_path");
        assert!(matches!(
            resolved,
            Some(ParameterSpec::Primitive {
                kind: TypeToken::String,
                ..
            })
        ));
    }

    #[test]
    fn resolve_parameter_path_nested() {
        let params = schema(r#"{ video: { device_path: "string", fps: "u16" } }"#);
        let resolved = resolve_parameter_path(&params, "video.device_path");
        assert!(matches!(
            resolved,
            Some(ParameterSpec::Primitive {
                kind: TypeToken::String,
                ..
            })
        ));
        let fps = resolve_parameter_path(&params, "video.fps");
        assert!(matches!(
            fps,
            Some(ParameterSpec::Primitive {
                kind: TypeToken::U16,
                ..
            })
        ));
    }

    #[test]
    fn resolve_parameter_path_not_found() {
        let params = schema(r#"{ device_path: "string" }"#);
        assert!(resolve_parameter_path(&params, "nonexistent").is_none());
        assert!(resolve_parameter_path(&params, "device_path.nested").is_none());
    }

    // ---- resolve_argument_path ----

    #[test]
    fn resolve_argument_path_descends_into_object_groups() {
        let args: BTreeMap<String, AnyType> = BTreeMap::from([(
            "video".to_string(),
            AnyType::Object(BTreeMap::from([(
                "device_path".to_string(),
                AnyType::String("/dev/video0".to_string()),
            )])),
        )]);
        assert_eq!(
            resolve_argument_path(&args, "video.device_path"),
            Some(&AnyType::String("/dev/video0".to_string()))
        );
        // A top-level leaf resolves without descending.
        let flat: BTreeMap<String, AnyType> =
            BTreeMap::from([("fps".to_string(), AnyType::UInt(30))]);
        assert_eq!(
            resolve_argument_path(&flat, "fps"),
            Some(&AnyType::UInt(30))
        );
    }

    #[test]
    fn resolve_argument_path_returns_none_for_missing_or_non_group() {
        let args: BTreeMap<String, AnyType> = BTreeMap::from([(
            "video".to_string(),
            AnyType::String("not-an-object".to_string()),
        )]);
        // Missing top-level key.
        assert!(resolve_argument_path(&args, "missing").is_none());
        // Descending past a non-Object leaf yields None rather than panicking.
        assert!(resolve_argument_path(&args, "video.device_path").is_none());
    }

    // ---- validate_node_arguments (programmatic map entry point) ----

    #[test]
    fn validate_node_arguments_accepts_well_typed_map() {
        let s = schema(r#"{ count: "u16" }"#);
        let args =
            validate_node_arguments(BTreeMap::from([("count".to_string(), AnyType::Int(5))]), &s)
                .expect("valid args should resolve");
        assert!(args.0.contains_key("count"));
    }

    #[test]
    fn validate_node_arguments_rejects_type_mismatch() {
        let s = schema(r#"{ count: "u16" }"#);
        let err = validate_node_arguments(
            BTreeMap::from([("count".to_string(), AnyType::String("nope".to_string()))]),
            &s,
        )
        .expect_err("string for u16 should fail");
        assert!(matches!(err, NodeArgumentsError::TypeMismatch(_)));
    }

    #[test]
    fn validate_node_arguments_rejects_unknown_key() {
        let s = schema(r#"{ count: "u16" }"#);
        let err = validate_node_arguments(
            BTreeMap::from([
                ("count".to_string(), AnyType::Int(1)),
                ("bogus".to_string(), AnyType::Int(2)),
            ]),
            &s,
        )
        .expect_err("unknown key should fail");
        assert!(matches!(err, NodeArgumentsError::UnknownParameter { .. }));
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

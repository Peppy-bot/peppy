//! The restart policy an instance runs under: the values a level writes
//! under `restart`, where the resolved policy came from, and the say a
//! launcher or a flag hands the daemon.

use std::fmt::{self, Formatter};
use std::num::NonZeroU32;
use std::str::FromStr;

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, de};

use crate::error::ParsingError;

/// What the daemon does when the instance's process exits on its own;
/// `on_failure` with nothing declared.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RestartPolicy {
    /// The instance stays `finished` or `failed`.
    Never,
    /// The instance is started again after an exit that makes it `failed`:
    /// a non-zero code, a signal, or the health monitor's kill.
    #[default]
    OnFailure,
    /// The instance is started again after any exit, a clean one included.
    Always,
}

impl RestartPolicy {
    /// The accepted spellings, quoted in the refusal of any other value.
    pub const ACCEPTED: &'static str = "`never`, `on_failure` or `always`";

    pub fn as_str(self) -> &'static str {
        match self {
            RestartPolicy::Never => "never",
            RestartPolicy::OnFailure => "on_failure",
            RestartPolicy::Always => "always",
        }
    }

    /// Whether a failed exit (a non-zero code, a signal, a health kill)
    /// starts the instance again.
    pub fn restarts_after_failure(self) -> bool {
        matches!(self, RestartPolicy::OnFailure | RestartPolicy::Always)
    }

    /// Whether a clean exit (code 0) starts the instance again.
    pub fn restarts_after_clean_exit(self) -> bool {
        matches!(self, RestartPolicy::Always)
    }
}

impl FromStr for RestartPolicy {
    type Err = ParsingError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "never" => Ok(RestartPolicy::Never),
            "on_failure" => Ok(RestartPolicy::OnFailure),
            "always" => Ok(RestartPolicy::Always),
            other => Err(ParsingError::InvalidRestartPolicy {
                value: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for RestartPolicy {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for RestartPolicy {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PolicyVisitor;

        impl Visitor<'_> for PolicyVisitor {
            type Value = RestartPolicy;

            fn expecting(&self, f: &mut Formatter<'_>) -> fmt::Result {
                f.write_str(RestartPolicy::ACCEPTED)
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                RestartPolicy::from_str(value).map_err(|e| E::custom(e.to_string()))
            }
        }

        deserializer.deserialize_str(PolicyVisitor)
    }
}

/// What one level writes under `restart`: a policy and, when the level caps
/// it, the attempts in a row before the instance stays failed. Spelled
/// `"on_failure"` or `{ policy: "on_failure", max_restarts: 5 }`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RestartDeclaration {
    policy: RestartPolicy,
    /// Attempts in a row before the instance stays failed; `None` is
    /// unlimited. [`RestartDeclaration::new`] refuses it with `never`.
    max_restarts: Option<NonZeroU32>,
}

/// The value of `max_restarts`, a count of attempts in a row from 1;
/// `key` names the surface the refusal quotes (`max_restarts` in a file,
/// `--max-restarts` on the command line).
pub fn parse_max_restarts(key: &'static str, value: &str) -> Result<NonZeroU32, ParsingError> {
    value
        .parse::<u32>()
        .ok()
        .and_then(NonZeroU32::new)
        .ok_or_else(|| ParsingError::InvalidMaxRestarts {
            key,
            value: value.to_owned(),
        })
}

/// The refusal of a cap written beside `never`, which takes no restarts to
/// cap; the one way [`RestartDeclaration::new`] fails.
#[derive(Debug, thiserror::Error)]
#[error("`max_restarts` takes a policy that restarts, `on_failure` or `always`; got `never`")]
pub struct MaxRestartsWithNever;

impl RestartDeclaration {
    /// The one way to a declaration with a cap, so a cap on `never` is
    /// refused wherever the value comes from: a file, the flags or the wire.
    pub fn new(
        policy: RestartPolicy,
        max_restarts: Option<NonZeroU32>,
    ) -> Result<Self, MaxRestartsWithNever> {
        if max_restarts.is_some() && policy == RestartPolicy::Never {
            return Err(MaxRestartsWithNever);
        }
        Ok(Self {
            policy,
            max_restarts,
        })
    }

    pub fn policy_only(policy: RestartPolicy) -> Self {
        Self {
            policy,
            max_restarts: None,
        }
    }

    pub fn policy(&self) -> RestartPolicy {
        self.policy
    }

    pub fn max_restarts(&self) -> Option<NonZeroU32> {
        self.max_restarts
    }

    /// The declaration as a launcher or a manifest spells it: `"on_failure"`,
    /// or `{ policy: "on_failure", max_restarts: 5 }`.
    pub fn launcher_spelling(&self) -> String {
        match self.max_restarts {
            None => format!("\"{}\"", self.policy),
            Some(max) => format!("{{ policy: \"{}\", max_restarts: {max} }}", self.policy),
        }
    }
}

impl fmt::Display for RestartDeclaration {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.policy.as_str())?;
        if let Some(max) = self.max_restarts {
            write!(f, ", max_restarts {max}")?;
        }
        Ok(())
    }
}

impl Serialize for RestartDeclaration {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        match self.max_restarts {
            None => serializer.serialize_str(self.policy.as_str()),
            Some(max) => {
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("policy", self.policy.as_str())?;
                map.serialize_entry("max_restarts", &max.get())?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for RestartDeclaration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            policy: Option<RestartPolicy>,
            max_restarts: Option<CountText>,
        }

        /// Any scalar written as `max_restarts`, kept as its text so a value
        /// that is not a count is refused in the count's words.
        struct CountText(String);

        impl<'de> Deserialize<'de> for CountText {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                struct CountVisitor;

                impl Visitor<'_> for CountVisitor {
                    type Value = CountText;

                    fn expecting(&self, f: &mut Formatter<'_>) -> fmt::Result {
                        f.write_str("a count of attempts in a row")
                    }

                    fn visit_i64<E: de::Error>(self, value: i64) -> Result<CountText, E> {
                        Ok(CountText(value.to_string()))
                    }

                    fn visit_u64<E: de::Error>(self, value: u64) -> Result<CountText, E> {
                        Ok(CountText(value.to_string()))
                    }

                    fn visit_f64<E: de::Error>(self, value: f64) -> Result<CountText, E> {
                        // Debug keeps the fraction, so `5.0` stays a refused
                        // `5.0`.
                        Ok(CountText(format!("{value:?}")))
                    }

                    fn visit_str<E: de::Error>(self, value: &str) -> Result<CountText, E> {
                        Ok(CountText(format!("\"{value}\"")))
                    }

                    fn visit_bool<E: de::Error>(self, value: bool) -> Result<CountText, E> {
                        Ok(CountText(value.to_string()))
                    }
                }

                deserializer.deserialize_any(CountVisitor)
            }
        }

        struct DeclarationVisitor;

        impl<'de> Visitor<'de> for DeclarationVisitor {
            type Value = RestartDeclaration;

            fn expecting(&self, f: &mut Formatter<'_>) -> fmt::Result {
                write!(
                    f,
                    "{}, or `{{ policy, max_restarts }}`",
                    RestartPolicy::ACCEPTED
                )
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                RestartPolicy::from_str(value)
                    .map(RestartDeclaration::policy_only)
                    .map_err(|e| E::custom(e.to_string()))
            }

            fn visit_map<A>(self, map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let structured = |error: ParsingError| de::Error::custom(error.to_string());
                let raw = Raw::deserialize(de::value::MapAccessDeserializer::new(map))?;
                let policy = raw
                    .policy
                    .ok_or_else(|| structured(ParsingError::RestartNeedsPolicy))?;
                let max_restarts = raw
                    .max_restarts
                    .map(|CountText(text)| parse_max_restarts("max_restarts", &text))
                    .transpose()
                    .map_err(structured)?;
                RestartDeclaration::new(policy, max_restarts)
                    .map_err(|refusal| de::Error::custom(refusal.to_string()))
            }
        }

        deserializer.deserialize_any(DeclarationVisitor)
    }
}

/// Where an instance's resolved restart policy was written.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartOrigin {
    /// The instance entry or an adjustment in the launcher.
    Instance,
    /// `--restart` of `peppy node run` or `peppy node add --run`.
    Flag,
    /// The node's own `peppy.json5`.
    Node,
    /// The launcher root.
    LauncherRoot,
    /// Nothing wrote a policy.
    #[default]
    Default,
}

impl RestartOrigin {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            RestartOrigin::Instance => "instance",
            RestartOrigin::Flag => "flag",
            RestartOrigin::Node => "node",
            RestartOrigin::LauncherRoot => "launcher_root",
            RestartOrigin::Default => "default",
        }
    }

    /// The phrase a listing prints beside the policy.
    fn describe(self) -> &'static str {
        match self {
            RestartOrigin::Instance => "instance entry",
            RestartOrigin::Flag => "--restart",
            RestartOrigin::Node => "peppy.json5",
            RestartOrigin::LauncherRoot => "launcher root",
            RestartOrigin::Default => "default",
        }
    }
}

impl FromStr for RestartOrigin {
    type Err = ParsingError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "instance" => Ok(RestartOrigin::Instance),
            "flag" => Ok(RestartOrigin::Flag),
            "node" => Ok(RestartOrigin::Node),
            "launcher_root" => Ok(RestartOrigin::LauncherRoot),
            "default" => Ok(RestartOrigin::Default),
            other => Err(ParsingError::UnknownRestartOrigin {
                value: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for RestartOrigin {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The launcher's or the flag's say on an instance's policy; the daemon
/// resolves it against the node's `peppy.json5`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum RestartRequest {
    #[default]
    Unset,
    /// The instance entry or an adjustment wrote it.
    Instance(RestartDeclaration),
    /// `--restart` of `peppy node run` or `peppy node add --run` wrote it.
    Flag(RestartDeclaration),
    /// The launcher root wrote it.
    LauncherRoot(RestartDeclaration),
}

impl RestartRequest {
    /// The policy this request writes, with the level that wrote it; `None`
    /// when it writes none.
    pub fn written(self) -> Option<ResolvedRestart> {
        let (declaration, origin) = match self {
            Self::Unset => return None,
            Self::Instance(declaration) => (declaration, RestartOrigin::Instance),
            Self::Flag(declaration) => (declaration, RestartOrigin::Flag),
            Self::LauncherRoot(declaration) => (declaration, RestartOrigin::LauncherRoot),
        };
        Some(ResolvedRestart {
            declaration,
            origin,
        })
    }
}

/// An instance's policy once every level has been consulted.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ResolvedRestart {
    pub declaration: RestartDeclaration,
    pub origin: RestartOrigin,
}

impl fmt::Display for ResolvedRestart {
    /// The policy with where it was written: `on_failure, max_restarts 5
    /// (instance entry)`.
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.declaration, self.origin.describe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_declaration(json5: &str) -> Result<RestartDeclaration, String> {
        serde_json5::from_str::<RestartDeclaration>(json5).map_err(|e| e.to_string())
    }

    #[test]
    fn restart_declaration_parses_the_string_and_the_object() {
        let short = parse_declaration(r#""on_failure""#).unwrap();
        assert_eq!(short.policy(), RestartPolicy::OnFailure);
        assert_eq!(short.max_restarts(), None);

        let capped = parse_declaration(r#"{ policy: "always", max_restarts: 5 }"#).unwrap();
        assert_eq!(capped.policy(), RestartPolicy::Always);
        assert_eq!(capped.max_restarts(), NonZeroU32::new(5));
        assert_eq!(capped.to_string(), "always, max_restarts 5");
        assert_eq!(short.to_string(), "on_failure");
    }

    #[test]
    fn restart_policy_rejects_a_bad_string_naming_the_values() {
        let err = parse_declaration(r#""sometimes""#).unwrap_err();
        assert!(
            err.contains("`restart` is `never`, `on_failure` or `always`; got `sometimes`"),
            "{err}"
        );
        let err = parse_declaration(r#"{ policy: "sometimes" }"#).unwrap_err();
        assert!(
            err.contains("`restart` is `never`, `on_failure` or `always`; got `sometimes`"),
            "{err}"
        );
    }

    #[test]
    fn restart_policy_rejects_a_number_naming_the_values() {
        let err = parse_declaration("1").unwrap_err();
        assert!(
            err.contains(
                "expected `never`, `on_failure` or `always`, or `{ policy, max_restarts }`"
            ),
            "{err}"
        );
        let err = parse_declaration(r#"{ policy: 1 }"#).unwrap_err();
        assert!(
            err.contains("expected `never`, `on_failure` or `always`"),
            "{err}"
        );
    }

    #[test]
    fn restart_declaration_rejects_zero_and_unknown_fields() {
        let err = parse_declaration(r#"{ policy: "on_failure", max_restarts: 0 }"#).unwrap_err();
        assert!(
            err.contains(
                "`max_restarts` is a count of attempts in a row, from 1 to 4294967295; got `0`"
            ),
            "{err}"
        );
        let err = parse_declaration(r#"{ policy: "on_failure", max_restarts: -2 }"#).unwrap_err();
        assert!(err.contains("got `-2`"), "{err}");
        let top = parse_declaration(r#"{ policy: "on_failure", max_restarts: 4294967295 }"#)
            .expect("the top of the range is a cap");
        assert_eq!(top.max_restarts().map(NonZeroU32::get), Some(u32::MAX));
        let err =
            parse_declaration(r#"{ policy: "on_failure", max_restarts: 4294967296 }"#).unwrap_err();
        assert!(
            err.contains("from 1 to 4294967295; got `4294967296`"),
            "{err}"
        );
        for (value, shown) in [
            ("2.5", "2.5"),
            ("5.0", "5.0"),
            (r#""5""#, r#""5""#),
            ("true", "true"),
        ] {
            let err = parse_declaration(&format!(
                r#"{{ policy: "on_failure", max_restarts: {value} }}"#
            ))
            .unwrap_err();
            assert!(
                err.contains(&format!(
                    "`max_restarts` is a count of attempts in a row, from 1 to 4294967295; got \
                     `{shown}`"
                )),
                "{value}: {err}"
            );
        }
        let err = parse_declaration(r#"{ policy: "on_failure", retries: 2 }"#).unwrap_err();
        assert!(
            err.contains("unknown field `retries`, expected `policy` or `max_restarts`"),
            "{err}"
        );
        let err = parse_declaration(r#"{ max_restarts: 2 }"#).unwrap_err();
        assert!(
            err.contains(
                "`restart` as an object needs `policy`: `never`, `on_failure` or `always`"
            ),
            "{err}"
        );
    }

    #[test]
    fn restart_declaration_refuses_never_with_a_cap() {
        let err = parse_declaration(r#"{ policy: "never", max_restarts: 2 }"#).unwrap_err();
        assert!(
            err.contains(
                "`max_restarts` takes a policy that restarts, `on_failure` or `always`; got `never`"
            ),
            "{err}"
        );
        assert!(RestartDeclaration::new(RestartPolicy::Never, NonZeroU32::new(2)).is_err());
        assert!(RestartDeclaration::new(RestartPolicy::Never, None).is_ok());
    }

    #[test]
    fn restart_declaration_serializes_the_short_form_without_a_cap() {
        let short = RestartDeclaration::policy_only(RestartPolicy::OnFailure);
        assert_eq!(serde_json::to_string(&short).unwrap(), r#""on_failure""#);
        let capped = RestartDeclaration::new(RestartPolicy::OnFailure, NonZeroU32::new(3)).unwrap();
        assert_eq!(
            serde_json::to_string(&capped).unwrap(),
            r#"{"policy":"on_failure","max_restarts":3}"#
        );
        let back: RestartDeclaration =
            serde_json::from_str(&serde_json::to_string(&capped).unwrap()).unwrap();
        assert_eq!(back, capped);
    }

    #[test]
    fn restart_origin_round_trips_its_ids() {
        for origin in [
            RestartOrigin::Instance,
            RestartOrigin::Flag,
            RestartOrigin::Node,
            RestartOrigin::LauncherRoot,
            RestartOrigin::Default,
        ] {
            assert_eq!(origin.as_str().parse::<RestartOrigin>().unwrap(), origin);
            assert_eq!(
                serde_json::to_string(&origin).unwrap(),
                format!("\"{}\"", origin.as_str())
            );
        }
        assert!("elsewhere".parse::<RestartOrigin>().is_err());
        assert_eq!(RestartOrigin::Node.describe(), "peppy.json5");
    }

    /// A resolved restart renders its declaration and its origin's words, as
    /// `stack resolve`, `stack list` and `node info` print it.
    #[test]
    fn resolved_restart_renders_its_declaration_and_origin() {
        assert_eq!(
            ResolvedRestart::default().to_string(),
            "on_failure (default)"
        );
        let capped = ResolvedRestart {
            declaration: RestartDeclaration::new(RestartPolicy::Always, NonZeroU32::new(5))
                .unwrap(),
            origin: RestartOrigin::Instance,
        };
        assert_eq!(
            capped.to_string(),
            "always, max_restarts 5 (instance entry)"
        );
    }

    /// A `peppy.json5` with a bad `restart` value is refused at its path,
    /// naming the values it takes.
    #[test]
    fn manifest_refuses_a_bad_restart_naming_the_values() {
        let manifest = |restart: &str| {
            format!(
                r#"{{
                    peppy_schema: "node/v1",
                    manifest: {{ name: "cam", tag: "v1", description: ["A test node."] }},
                    execution: {{ language: "rust", run_cmd: ["./cam"] }},
                    framework: {{ restart: {restart} }},
                }}"#
            )
        };
        let err = super::super::NodeConfigParser::from_content(&manifest("1"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(
                "invalid type: integer `1`, expected `never`, `on_failure` or `always`, or \
                 `{ policy, max_restarts }`"
            ),
            "{err}"
        );
        assert!(
            err.contains("framework.restart"),
            "the path is named: {err}"
        );
        let err = super::super::NodeConfigParser::from_content(&manifest(r#""sometimes""#))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`restart` is `never`, `on_failure` or `always`; got `sometimes`"),
            "{err}"
        );
    }

    #[test]
    fn restart_policy_restarts_after_an_exit() {
        assert!(!RestartPolicy::Never.restarts_after_clean_exit());
        assert!(!RestartPolicy::Never.restarts_after_failure());
        assert!(!RestartPolicy::OnFailure.restarts_after_clean_exit());
        assert!(RestartPolicy::OnFailure.restarts_after_failure());
        assert!(RestartPolicy::Always.restarts_after_clean_exit());
        assert!(RestartPolicy::Always.restarts_after_failure());
    }

    /// A request names the level it was written at, unset at none, and the
    /// resolved policy renders with that level's phrase.
    #[test]
    fn a_written_request_renders_with_its_level() {
        let capped = RestartDeclaration::new(RestartPolicy::OnFailure, NonZeroU32::new(5)).unwrap();
        assert_eq!(RestartRequest::Unset.written(), None);
        let rendered =
            |request: RestartRequest| request.written().map(|written| written.to_string());
        assert_eq!(
            rendered(RestartRequest::Instance(capped)).as_deref(),
            Some("on_failure, max_restarts 5 (instance entry)")
        );
        assert_eq!(
            rendered(RestartRequest::Flag(RestartDeclaration::policy_only(
                RestartPolicy::Always
            )))
            .as_deref(),
            Some("always (--restart)")
        );
        assert_eq!(
            rendered(RestartRequest::LauncherRoot(
                RestartDeclaration::policy_only(RestartPolicy::Never)
            ))
            .as_deref(),
            Some("never (launcher root)")
        );
    }

    #[test]
    fn restart_declaration_spells_itself_as_a_launcher_does() {
        assert_eq!(
            RestartDeclaration::policy_only(RestartPolicy::OnFailure).launcher_spelling(),
            r#""on_failure""#
        );
        assert_eq!(
            RestartDeclaration::new(RestartPolicy::Always, NonZeroU32::new(5))
                .unwrap()
                .launcher_spelling(),
            r#"{ policy: "always", max_restarts: 5 }"#
        );
    }
}

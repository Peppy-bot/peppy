use std::path::PathBuf;

use config::namespace::Namespace;
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

use crate::policy::stable_renewal_jitter;

/// Non-secret metadata mirrored in `credentials.json5` v1 and the protected
/// identity pointer. The PEM bodies and private key are deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoreNodeIdentity {
    pub api_origin: String,
    pub subject: String,
    /// Fresh OAuth login that authorized this enrollment. Daemon-PAT identities
    /// have no persisted session revision.
    #[serde(deserialize_with = "deserialize_optional_uuid")]
    pub session_revision: Option<Uuid>,
    pub workspace_id: Namespace,
    pub core_node_name: String,
    pub active_generation: String,
    pub serial_number: String,
    pub spki_sha256: String,
    pub not_before: i64,
    pub not_after: i64,
    pub renew_after: i64,
}

impl CoreNodeIdentity {
    pub fn is_valid_at(&self, now: i64) -> bool {
        now >= self.not_before && now < self.not_after
    }

    pub fn renewal_due(&self, now: i64) -> bool {
        now >= self.renewal_at()
    }

    /// Stable per-generation early-renewal threshold shared by eligibility and
    /// daemon scheduling. Using one value avoids a one-second busy loop in the
    /// jitter window and actually distributes fleet rotations.
    pub fn renewal_at(&self) -> i64 {
        self.renew_after
            .saturating_sub(stable_renewal_jitter(&self.active_generation))
    }
}

/// Generation-specific files consumed by Zenoh. Paths change on every key
/// rotation so desired-state equality necessarily triggers a router reload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityPaths {
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    pub generation: String,
    pub workspace_id: Option<Namespace>,
}

/// Durable description of a locally generated key that has not yet received
/// a certificate. It contains no bearer token or private-key bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingEnrollment {
    pub api_origin: String,
    pub subject: String,
    pub core_node_name: String,
    pub generation: String,
    pub spki_sha256: String,
}

/// Non-secret durable intent proving that an accepted logout must finish
/// fail-closed after a process crash. `None` is the explicit no-session case;
/// the field remains required on the strict v1 shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogoutIntent {
    pub version: u8,
    #[serde(deserialize_with = "deserialize_optional_uuid")]
    pub expected_session_revision: Option<Uuid>,
}

fn deserialize_optional_uuid<'de, D>(deserializer: D) -> Result<Option<Uuid>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<Uuid>::deserialize(deserializer)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORKSPACE: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn identity() -> CoreNodeIdentity {
        CoreNodeIdentity {
            api_origin: "https://api.peppy.bot".into(),
            subject: "user-test-subject".into(),
            session_revision: Some(
                Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap(),
            ),
            workspace_id: Namespace::parse(WORKSPACE).unwrap(),
            core_node_name: "core-node-test-0001".into(),
            active_generation: "a".repeat(64),
            serial_number: "01".into(),
            spki_sha256: "a".repeat(64),
            not_before: 1_000,
            not_after: 3_000,
            renew_after: 2_000,
        }
    }

    #[test]
    fn validity_interval_is_half_open() {
        let identity = identity();

        assert!(!identity.is_valid_at(identity.not_before - 1));
        assert!(identity.is_valid_at(identity.not_before));
        assert!(identity.is_valid_at(identity.not_after - 1));
        assert!(!identity.is_valid_at(identity.not_after));
    }

    #[test]
    fn renewal_eligibility_uses_the_same_stable_jittered_threshold() {
        let identity = identity();
        let threshold = identity.renewal_at();

        assert!(threshold <= identity.renew_after);
        assert!(!identity.renewal_due(threshold - 1));
        assert!(identity.renewal_due(threshold));
    }

    #[test]
    fn serialized_identity_shape_is_stable_and_requires_the_revision_field() {
        let identity = identity();
        let mut value = serde_json::to_value(&identity).unwrap();

        assert_eq!(
            value,
            serde_json::json!({
                "api_origin": "https://api.peppy.bot",
                "subject": "user-test-subject",
                "session_revision": "11111111-1111-4111-8111-111111111111",
                "workspace_id": WORKSPACE,
                "core_node_name": "core-node-test-0001",
                "active_generation": "a".repeat(64),
                "serial_number": "01",
                "spki_sha256": "a".repeat(64),
                "not_before": 1_000,
                "not_after": 3_000,
                "renew_after": 2_000,
            })
        );
        assert_eq!(
            serde_json::from_value::<CoreNodeIdentity>(value.clone()).unwrap(),
            identity
        );

        value.as_object_mut().unwrap().remove("session_revision");
        assert!(
            serde_json::from_value::<CoreNodeIdentity>(value).is_err(),
            "the clean-break identity shape must not accept pre-revision metadata"
        );
    }
}

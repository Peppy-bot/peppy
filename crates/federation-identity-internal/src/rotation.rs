use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    CoreNodeIdentity, IdentityError, IdentityLock, IdentityResult, IdentityStore,
    validate_identity_metadata_shape,
};

const ROTATION_RECEIPT_VERSION: u32 = 1;

/// Opaque ownership token for one durably published, not-yet-verified
/// generation. Its serialized form is the on-disk v1 rotation receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RotationRecord {
    receipt_version: u32,
    receipt_id: String,
    previous: Option<CoreNodeIdentity>,
    activated: CoreNodeIdentity,
}

impl RotationRecord {
    pub fn previous(&self) -> Option<&CoreNodeIdentity> {
        self.previous.as_ref()
    }

    pub fn activated(&self) -> &CoreNodeIdentity {
        &self.activated
    }

    pub fn receipt_id(&self) -> &str {
        &self.receipt_id
    }

    pub(crate) fn validate(&self) -> IdentityResult<()> {
        if self.receipt_version != ROTATION_RECEIPT_VERSION {
            return Err(IdentityError::invalid(format!(
                "unsupported core-node rotation receipt version {}",
                self.receipt_version
            )));
        }
        let id = Uuid::parse_str(&self.receipt_id).map_err(|error| {
            IdentityError::invalid(format!("invalid core-node rotation receipt id: {error}"))
        })?;
        if id.hyphenated().to_string() != self.receipt_id {
            return Err(IdentityError::invalid(
                "core-node rotation receipt id is not a canonical UUID",
            ));
        }
        validate_identity_metadata_shape(&self.activated)?;
        if let Some(previous) = self.previous.as_ref() {
            validate_identity_metadata_shape(previous)?;
        }
        Ok(())
    }
}

/// Pointer publication selected during rollback. The credentials mirror must
/// be changed to `previous` before the receipt is removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackPublication {
    previous: Option<CoreNodeIdentity>,
    rejected_generation: Option<String>,
}

impl RollbackPublication {
    pub fn previous(&self) -> Option<&CoreNodeIdentity> {
        self.previous.as_ref()
    }

    pub fn previous_owned(&self) -> Option<CoreNodeIdentity> {
        self.previous.clone()
    }

    pub fn rejected_generation(&self) -> Option<&str> {
        self.rejected_generation.as_deref()
    }
}

impl IdentityStore {
    /// Reports whether a valid durable rotation receipt still needs an owner.
    /// Reading an activated pointer without either this receipt's armed guard
    /// or a completed commit is never sufficient proof that the generation may
    /// be applied.
    pub fn unverified_rotation_pending(&self, lock: &IdentityLock) -> IdentityResult<bool> {
        let Some(record) = self.read_receipt::<RotationRecord>(lock)? else {
            return Ok(false);
        };
        record.validate()?;
        Ok(true)
    }

    /// Writes rollback intent and then publishes the activated canonical
    /// pointer. The caller mirrors credentials only after this succeeds.
    pub fn begin_rotation(
        &self,
        lock: &IdentityLock,
        previous: Option<CoreNodeIdentity>,
        activated: CoreNodeIdentity,
    ) -> IdentityResult<RotationRecord> {
        validate_identity_metadata_shape(&activated)?;
        if let Some(previous) = previous.as_ref() {
            validate_identity_metadata_shape(previous)?;
        }
        let record = RotationRecord {
            receipt_version: ROTATION_RECEIPT_VERSION,
            receipt_id: Uuid::new_v4().hyphenated().to_string(),
            previous,
            activated,
        };
        self.write_receipt(lock, &record)?;
        self.publish_pointer(lock, Some(&record.activated))?;
        Ok(record)
    }

    pub fn finish_rotation_publication(
        &self,
        lock: &IdentityLock,
        record: &RotationRecord,
    ) -> IdentityResult<()> {
        self.verify_rotation(lock, record, Some(&record.activated))?;
        self.finish_pending(lock)
    }

    pub fn commit_rotation(
        &self,
        lock: &IdentityLock,
        record: &RotationRecord,
    ) -> IdentityResult<()> {
        self.finalize_rotation(lock, record)?;
        self.prune_generations(lock, &record.activated.active_generation)
    }

    /// Verifies the exact live receipt/pointer pair and durably removes the
    /// receipt. Generation pruning is deliberately separate: once this step
    /// succeeds the rotation is committed, and a later prune failure is only
    /// cleanup debt rather than an ambiguous identity transaction.
    pub fn finalize_rotation(
        &self,
        lock: &IdentityLock,
        record: &RotationRecord,
    ) -> IdentityResult<()> {
        self.verify_rotation(lock, record, Some(&record.activated))?;
        self.remove_receipt(lock)
    }

    /// Completes a rotation Peppy cannot prove an operator-managed router has
    /// consumed. The active pointer is durable and the receipt is removed, but
    /// no generation is pruned: an external router may still reference any
    /// previously published immutable path until its operator updates it.
    pub fn commit_operator_managed_rotation(
        &self,
        lock: &IdentityLock,
        record: &RotationRecord,
    ) -> IdentityResult<()> {
        self.finalize_rotation(lock, record)
    }

    pub fn retain_rotation(
        &self,
        lock: &IdentityLock,
        record: &RotationRecord,
    ) -> IdentityResult<()> {
        self.verify_rotation(lock, record, Some(&record.activated))
    }

    /// Selects only a still-valid, fully verified previous generation and
    /// publishes it. The receipt remains until the caller mirrors credentials.
    pub fn prepare_rollback(
        &self,
        lock: &IdentityLock,
        record: &RotationRecord,
        now: i64,
    ) -> IdentityResult<RollbackPublication> {
        self.verify_rotation(lock, record, Some(&record.activated))?;
        let previous = record.previous.clone().filter(|previous| {
            previous.is_valid_at(now) && self.validate_stored_material(previous, now).is_ok()
        });
        self.publish_pointer(lock, previous.as_ref())?;
        let rejected_generation = previous
            .as_ref()
            .is_none_or(|previous| previous.active_generation != record.activated.active_generation)
            .then(|| record.activated.active_generation.clone());
        Ok(RollbackPublication {
            previous,
            rejected_generation,
        })
    }

    pub fn finish_rollback(
        &self,
        lock: &IdentityLock,
        record: &RotationRecord,
        publication: &RollbackPublication,
    ) -> IdentityResult<()> {
        self.verify_rotation(lock, record, publication.previous.as_ref())?;
        self.remove_receipt(lock)
    }

    /// Removes a receipt after an activation mirror failure was durably
    /// restored to its previous value in both canonical locations.
    pub fn cancel_rotation(
        &self,
        lock: &IdentityLock,
        record: &RotationRecord,
    ) -> IdentityResult<()> {
        self.verify_rotation(lock, record, record.previous.as_ref())?;
        self.remove_receipt(lock)
    }

    pub(crate) fn verify_rotation(
        &self,
        lock: &IdentityLock,
        expected: &RotationRecord,
        expected_pointer: Option<&CoreNodeIdentity>,
    ) -> IdentityResult<()> {
        let persisted = self.read_receipt::<RotationRecord>(lock)?.ok_or_else(|| {
            IdentityError::invalid(
                "core-node rotation receipt disappeared before its terminal operation",
            )
        })?;
        persisted.validate()?;
        if &persisted != expected {
            return Err(IdentityError::invalid(
                "core-node rotation receipt ownership changed; refusing a stale commit/rollback",
            ));
        }
        if self.read_pointer(lock)?.as_ref() != expected_pointer {
            return Err(IdentityError::invalid(
                "core-node rotation pointer changed; refusing a stale commit/rollback",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use config::namespace::Namespace;

    use super::*;

    fn identity(generation: char, not_after: i64) -> CoreNodeIdentity {
        let generation = generation.to_string().repeat(64);
        CoreNodeIdentity {
            api_origin: "https://api.peppy.bot".into(),
            subject: "user-test-subject".into(),
            session_revision: None,
            workspace_id: Namespace::parse("550e8400-e29b-41d4-a716-446655440000").unwrap(),
            core_node_name: "core-node-test-0001".into(),
            active_generation: generation.clone(),
            serial_number: "01".into(),
            spki_sha256: generation,
            not_before: 1,
            renew_after: 2,
            not_after,
        }
    }

    #[test]
    fn commit_removes_receipt_and_keeps_activated_pointer() {
        let temp = tempfile::tempdir().unwrap();
        let store = IdentityStore::new(temp.path());
        let lock = store.acquire_lock().unwrap();
        store.ensure_private_layout(&lock).unwrap();
        let activated = identity('b', 100);
        let record = store
            .begin_rotation(&lock, Some(identity('a', 100)), activated.clone())
            .unwrap();

        store.commit_rotation(&lock, &record).unwrap();

        assert_eq!(store.read_pointer(&lock).unwrap(), Some(activated));
        assert!(
            store
                .read_receipt::<RotationRecord>(&lock)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn operator_managed_commit_retains_previous_generation() {
        let temp = tempfile::tempdir().unwrap();
        let store = IdentityStore::new(temp.path());
        let lock = store.acquire_lock().unwrap();
        store.ensure_private_layout(&lock).unwrap();
        let previous = identity('a', 100);
        let activated = identity('b', 100);
        std::fs::create_dir_all(store.generation_dir(&previous.active_generation)).unwrap();
        std::fs::create_dir_all(store.generation_dir(&activated.active_generation)).unwrap();
        let record = store
            .begin_rotation(&lock, Some(previous.clone()), activated.clone())
            .unwrap();

        store
            .commit_operator_managed_rotation(&lock, &record)
            .unwrap();

        assert_eq!(store.read_pointer(&lock).unwrap(), Some(activated));
        assert!(store.generation_dir(&previous.active_generation).is_dir());
        assert!(
            store
                .read_receipt::<RotationRecord>(&lock)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rollback_never_restores_expired_previous_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let store = IdentityStore::new(temp.path());
        let lock = store.acquire_lock().unwrap();
        store.ensure_private_layout(&lock).unwrap();
        let record = store
            .begin_rotation(&lock, Some(identity('a', 5)), identity('b', 100))
            .unwrap();

        let publication = store.prepare_rollback(&lock, &record, 10).unwrap();
        assert!(publication.previous().is_none());
        assert_eq!(
            publication.rejected_generation(),
            Some("b".repeat(64).as_str())
        );
        store.finish_rollback(&lock, &record, &publication).unwrap();
        assert!(store.read_pointer(&lock).unwrap().is_none());
    }

    #[test]
    fn stale_record_cannot_commit_another_rotation() {
        let temp = tempfile::tempdir().unwrap();
        let store = IdentityStore::new(temp.path());
        let lock = store.acquire_lock().unwrap();
        store.ensure_private_layout(&lock).unwrap();
        let stale = store
            .begin_rotation(&lock, None, identity('a', 100))
            .unwrap();
        let current = store
            .begin_rotation(&lock, Some(identity('a', 100)), identity('b', 100))
            .unwrap();

        assert!(store.commit_rotation(&lock, &stale).is_err());
        store.commit_rotation(&lock, &current).unwrap();
    }
}

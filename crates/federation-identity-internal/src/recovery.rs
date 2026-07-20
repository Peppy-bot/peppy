use crate::{
    CoreNodeIdentity, IdentityError, IdentityLock, IdentityResult, IdentityStore, RotationRecord,
};

/// Pointer state selected from a valid durable rotation receipt after a crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryTarget {
    Activated,
    Previous,
}

/// Transport-independent recovery instruction. The caller first reconciles
/// its credentials mirror to `mirror_identity`, then calls `finish_recovery`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryPlan {
    target: RecoveryTarget,
    rotation: RotationRecord,
}

impl RecoveryPlan {
    pub fn target(&self) -> RecoveryTarget {
        self.target
    }

    pub fn rotation(&self) -> &RotationRecord {
        &self.rotation
    }

    pub fn mirror_identity(&self) -> Option<&CoreNodeIdentity> {
        match self.target {
            RecoveryTarget::Activated => Some(self.rotation.activated()),
            RecoveryTarget::Previous => self.rotation.previous(),
        }
    }
}

impl IdentityStore {
    pub fn recovery_plan(&self, lock: &IdentityLock) -> IdentityResult<Option<RecoveryPlan>> {
        let Some(rotation) = self.read_receipt::<RotationRecord>(lock)? else {
            return Ok(None);
        };
        rotation.validate()?;
        let pointer = self.read_pointer(lock)?;
        let target = if pointer.as_ref() == Some(rotation.activated()) {
            RecoveryTarget::Activated
        } else if pointer.as_ref() == rotation.previous() {
            RecoveryTarget::Previous
        } else {
            return Err(IdentityError::invalid(
                "unverified core-node rotation metadata does not match the active identity pointer",
            ));
        };
        Ok(Some(RecoveryPlan { target, rotation }))
    }

    pub fn finish_recovery(&self, lock: &IdentityLock, plan: &RecoveryPlan) -> IdentityResult<()> {
        self.verify_rotation(lock, &plan.rotation, plan.mirror_identity())?;
        match plan.target {
            RecoveryTarget::Activated => self.finish_pending(lock),
            RecoveryTarget::Previous => {
                self.remove_receipt(lock)?;
                self.finish_pending(lock)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use config::namespace::Namespace;

    use super::*;

    fn identity(generation: char) -> CoreNodeIdentity {
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
            not_after: 100,
        }
    }

    #[test]
    fn activated_pointer_recovers_an_armed_rotation() {
        let temp = tempfile::tempdir().unwrap();
        let store = IdentityStore::new(temp.path());
        let lock = store.acquire_lock().unwrap();
        store.ensure_private_layout(&lock).unwrap();
        let activated = identity('b');
        let record = store
            .begin_rotation(&lock, Some(identity('a')), activated.clone())
            .unwrap();

        let plan = store.recovery_plan(&lock).unwrap().unwrap();
        assert_eq!(plan.target(), RecoveryTarget::Activated);
        assert_eq!(plan.mirror_identity(), Some(&activated));
        store.finish_recovery(&lock, &plan).unwrap();
        assert_eq!(plan.rotation(), &record);
        assert!(
            store
                .read_receipt::<RotationRecord>(&lock)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn previous_pointer_finishes_an_interrupted_rollback() {
        let temp = tempfile::tempdir().unwrap();
        let store = IdentityStore::new(temp.path());
        let lock = store.acquire_lock().unwrap();
        store.ensure_private_layout(&lock).unwrap();
        let previous = identity('a');
        let record = store
            .begin_rotation(&lock, Some(previous.clone()), identity('b'))
            .unwrap();
        store.publish_pointer(&lock, Some(&previous)).unwrap();

        let plan = store.recovery_plan(&lock).unwrap().unwrap();
        assert_eq!(plan.target(), RecoveryTarget::Previous);
        assert_eq!(plan.mirror_identity(), Some(&previous));
        store.finish_recovery(&lock, &plan).unwrap();
        assert!(
            store
                .read_receipt::<RotationRecord>(&lock)
                .unwrap()
                .is_none()
        );
        assert_eq!(record.previous(), Some(&previous));
    }

    #[test]
    fn unrelated_pointer_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let store = IdentityStore::new(temp.path());
        let lock = store.acquire_lock().unwrap();
        store.ensure_private_layout(&lock).unwrap();
        store
            .begin_rotation(&lock, Some(identity('a')), identity('b'))
            .unwrap();
        store.publish_pointer(&lock, Some(&identity('c'))).unwrap();

        assert!(store.recovery_plan(&lock).is_err());
    }
}

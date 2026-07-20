//! Certificate identity models and mechanics for Peppy federation.
//!
//! This private crate is intentionally independent of authenticated backend
//! communication, OAuth credentials, daemon orchestration, and router control.

#![forbid(unsafe_code)]

mod crypto;
mod model;
mod policy;
mod recovery;
mod rotation;
mod store;

pub use crypto::{
    CryptoError, EnrollmentRequest, InspectedLeaf, KeyPair, MAX_LEAF_VALIDITY_SECS,
    ReturnedCertificate, build_csr, generate_private_key, identity_uri, inspect_leaf,
    is_valid_positive_der_serial, normalize_serial, parse_private_key_pem, spki_fingerprint,
    validate_returned_certificate,
};
pub use model::{CoreNodeIdentity, IdentityPaths, LogoutIntent, PendingEnrollment};
pub use recovery::{RecoveryPlan, RecoveryTarget};
pub use rotation::{RollbackPublication, RotationRecord};
pub use store::{
    BindingTransition, IdentityError, IdentityLock, IdentityOwnerGuard, IdentityResult,
    IdentityStore, PendingGeneration, RotationLease, acquire_identity_owner, normalize_api_origin,
    validate_core_node_name, validate_generation_name, validate_identity_metadata_shape,
};

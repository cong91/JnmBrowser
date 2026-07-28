//! Shared runtime launch policy and process-local profile leases for automation.
//!
//! Option C from the browser-profile reuse plan: immutable source profiles with
//! task-local `LaunchPolicy`, exclusive `LeaseRegistry`, and RAII `LeaseCleanupGuard`.

pub mod guard;
pub mod lease;
pub mod policy;

pub use guard::LeaseCleanupGuard;
pub use lease::{LeaseError, LeaseRegistry, RuntimeLease};
pub use policy::{DataMode, FingerprintMode, LaunchPolicy};

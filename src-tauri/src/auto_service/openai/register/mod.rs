pub mod commands;
pub mod engine;
pub mod sentinel;
pub mod store;
pub mod task;
pub mod types;

#[cfg(test)]
mod live_harness;

// Re-export shared utilities that used to live in this module.
pub use crate::auto_service::common::{nord_cli, totp};

#[allow(unused_imports)]
pub use engine::RegistrationEngine;
#[allow(unused_imports)]
pub use store::{
  delete_cdk_inventory, delete_registered_account, list_cdk_inventory, list_registered_accounts,
  save_registration_result,
};
#[allow(unused_imports)]
pub use types::{
  CdkInventoryRecord, NetworkMode, RegistrationConfig, RegistrationProgress, RegistrationResult,
  RegistrationStep,
};

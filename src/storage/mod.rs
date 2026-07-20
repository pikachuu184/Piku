pub mod local;
pub mod provider;

use std::sync::{Arc, OnceLock};

use crate::storage::local::LocalProvider;
use crate::storage::provider::StorageProvider;

static LOCAL: OnceLock<Arc<LocalProvider>> = OnceLock::new();

/// The process-wide local storage provider. Cloud providers (MinIO, R2, …)
/// will slot in beside this behind the same [`StorageProvider`] trait.
pub fn local() -> Arc<LocalProvider> {
    LOCAL.get_or_init(|| Arc::new(LocalProvider::new())).clone()
}

/// The local provider as a trait object, for code written against the
/// provider abstraction.
pub fn local_dyn() -> Arc<dyn StorageProvider> {
    local()
}

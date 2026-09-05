use std::path::Path;

use crate::application::{DoctorStorageError, DoctorStorageInspector};

#[derive(Clone, Copy, Debug, Default)]
pub struct LocalFilesystemInspector;

impl DoctorStorageInspector for LocalFilesystemInspector {
    fn available_bytes(&self, path: &Path) -> Result<u64, DoctorStorageError> {
        let existing = path
            .ancestors()
            .find(|candidate| candidate.exists())
            .ok_or(DoctorStorageError::Unavailable)?;

        fs4::available_space(existing).map_err(|_| DoctorStorageError::Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn inspects_the_existing_parent_without_creating_the_cache_directory() {
        let temp = TempDir::new().unwrap();
        let cache = temp.path().join("nested/cache");

        let available = LocalFilesystemInspector.available_bytes(&cache).unwrap();

        assert!(available > 0);
        assert!(!cache.exists());
    }
}

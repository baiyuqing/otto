//! Source-compatible local session constructors backed by the POSIX repository.

use std::path::Path;
use std::sync::Arc;

use otto_core::session::{Header, PiError, Warning};

use super::lease::new_keeper;
use super::{ArchiveResult, Prepared, Store, prepared::archive_with_driver_and_keeper};
use otto_platform::fsops::files::FileDriver;

impl Store {
    pub fn create(root: impl AsRef<Path>, header: Header) -> Result<Self, PiError> {
        let store = Self::create_lazy(root, header)?;
        store.lock()?.ensure_file()?;
        Ok(store)
    }

    pub fn create_lazy(root: impl AsRef<Path>, header: Header) -> Result<Self, PiError> {
        Self::create_lazy_with_driver(root, header, Arc::new(otto_platform::fsops::PosixDriver))
    }

    pub fn create_lazy_with_driver(
        root: impl AsRef<Path>,
        header: Header,
        driver: Arc<dyn FileDriver>,
    ) -> Result<Self, PiError> {
        Self::create_lazy_with_driver_and_keeper(root, header, driver, new_keeper())
    }

    pub fn create_child_lazy(
        parent: impl AsRef<Path>,
        name: &str,
        header: Header,
    ) -> Result<Self, PiError> {
        Self::create_child_lazy_with_driver(
            parent,
            name,
            header,
            Arc::new(otto_platform::fsops::PosixDriver),
        )
    }

    pub fn create_child_lazy_with_driver(
        parent: impl AsRef<Path>,
        name: &str,
        header: Header,
        driver: Arc<dyn FileDriver>,
    ) -> Result<Self, PiError> {
        Self::create_child_lazy_with_driver_and_keeper(parent, name, header, driver, new_keeper())
    }

    pub fn open(path: impl AsRef<Path>) -> Result<(Self, Vec<Warning>), PiError> {
        Prepared::prepare(path.as_ref(), None)?.activate()
    }
}

impl Prepared {
    pub fn prepare(path: &Path, create_lease: Option<u64>) -> Result<Self, PiError> {
        Self::prepare_with_driver(
            path,
            create_lease,
            Arc::new(otto_platform::fsops::PosixDriver),
        )
    }

    pub fn prepare_with_driver(
        path: &Path,
        create_lease: Option<u64>,
        driver: Arc<dyn FileDriver>,
    ) -> Result<Self, PiError> {
        Self::prepare_with_driver_and_keeper(path, create_lease, driver, new_keeper())
    }

    pub fn prepare_listed(
        root: &Path,
        workspace: &str,
        path: &Path,
        create_lease: Option<u64>,
    ) -> Result<Self, PiError> {
        Self::prepare_listed_with_driver(
            root,
            workspace,
            path,
            create_lease,
            Arc::new(otto_platform::fsops::PosixDriver),
        )
    }

    pub fn prepare_listed_with_driver(
        root: &Path,
        workspace: &str,
        path: &Path,
        create_lease: Option<u64>,
        driver: Arc<dyn FileDriver>,
    ) -> Result<Self, PiError> {
        Self::prepare_listed_with_driver_and_keeper(
            root,
            workspace,
            path,
            create_lease,
            driver,
            new_keeper(),
        )
    }
}

pub fn archive(root: &Path, workspace: &str, path: &Path) -> Result<ArchiveResult, PiError> {
    archive_with_driver_and_keeper(
        root,
        workspace,
        path,
        Arc::new(otto_platform::fsops::PosixDriver),
        new_keeper(),
    )
}

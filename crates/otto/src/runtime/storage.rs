//! The composition boundary for local storage. Services receive domain ports.
use std::path::Path;
use std::sync::Arc;

use crate::approval::repository::ApprovalStore;
use crate::memory::advice::audit::AuditStore;
use crate::schedule::ScheduleStore;
use crate::session::SessionRepository;
use crate::skill::check_repository::CheckStore;
use crate::storage::{Connection, OpenOptions, OpenPolicy, SqlDriver, SqlRequirements};
use crate::subagent::record::TaskRepository;
use crate::usage::repository::UsageRepository;
use crate::workflow::WorkflowStore;

/// Opens the current repository schemas with any conforming SQLite-profile driver.
pub struct SqlRepositories {
    driver: Arc<dyn SqlDriver>,
}

impl SqlRepositories {
    pub fn new(driver: Arc<dyn SqlDriver>) -> Self {
        Self { driver }
    }

    /// Opens the database at `path`, creating it when missing.
    fn connection(&self, path: &Path) -> crate::storage::Result<Connection> {
        let requirements = if path == Path::new(":memory:") {
            SqlRequirements::EPHEMERAL
        } else {
            SqlRequirements::DURABLE
        };
        Connection::open_with_driver(
            self.driver.as_ref(),
            path,
            OpenOptions {
                read_only: false,
                policy: OpenPolicy::CreateIfMissing,
            },
            requirements,
        )
    }

    pub fn approvals(&self, path: &Path) -> crate::approval::Result<Arc<dyn ApprovalStore>> {
        let connection = self
            .connection(path)
            .map_err(|_| crate::approval::Error::Unavailable)?;
        Ok(Arc::new(
            crate::storage::backends::sqlite::approval::Store::from_connection(connection)?,
        ))
    }
    pub fn schedule(&self, path: &Path) -> crate::schedule::Result<Arc<dyn ScheduleStore>> {
        let connection = self.connection(path).map_err(|_| crate::schedule::Error)?;
        Ok(Arc::new(
            crate::storage::backends::sqlite::schedule::SqliteScheduleStore::from_connection(
                connection,
            )?,
        ))
    }
    pub fn workflow(&self, path: &Path) -> Result<Arc<dyn WorkflowStore>, String> {
        let connection = self
            .connection(path)
            .map_err(|_| "workflow storage is unavailable".to_owned())?;
        Ok(Arc::new(
            crate::storage::backends::sqlite::workflow::SqliteWorkflowStore::from_connection(
                connection,
            )?,
        ))
    }
    pub fn usage(&self, path: &Path) -> crate::usage::Result<Arc<dyn UsageRepository>> {
        let connection = self.connection(path).map_err(|_| crate::usage::Error)?;
        Ok(Arc::new(
            crate::storage::backends::sqlite::usage::Store::from_connection(connection)?,
        ))
    }
    pub fn tasks(&self, path: &Path) -> crate::subagent::record::Result<Arc<dyn TaskRepository>> {
        let connection = self
            .connection(path)
            .map_err(|_| crate::subagent::record::Error)?;
        Ok(Arc::new(
            crate::storage::backends::sqlite::tasks::Store::from_connection(connection)?,
        ))
    }
    pub fn skill_checks(
        &self,
        path: &Path,
    ) -> crate::skill::check_repository::StoreResult<Arc<dyn CheckStore>> {
        let connection = self
            .connection(path)
            .map_err(|_| crate::skill::check_repository::StoreError::Io)?;
        Ok(Arc::new(
            crate::storage::backends::sqlite::skill_checks::Store::from_connection(connection)?,
        ))
    }
    pub fn memory_advice(
        &self,
        path: &Path,
    ) -> crate::memory::advice::audit::Result<Arc<dyn AuditStore>> {
        let connection = self
            .connection(path)
            .map_err(|_| crate::memory::advice::audit::Error::Io)?;
        Ok(Arc::new(
            crate::storage::backends::sqlite::memory_advice::Store::from_connection(connection)?,
        ))
    }
}

/// Domain repositories at their local paths plus the POSIX session repository.
pub struct Storage {
    repositories: SqlRepositories,
    sessions: Arc<dyn SessionRepository>,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            repositories: SqlRepositories::new(Arc::new(
                crate::storage::drivers::sqlite::SqliteDriver,
            )),
            sessions: Arc::new(
                crate::storage::backends::posix::session::PosixSessionRepository::new(
                    Arc::new(crate::storage::drivers::posix::PosixDriver),
                    crate::failover::lease::new_keeper(),
                ),
            ),
        }
    }
}

impl Storage {
    pub fn sessions(&self) -> &dyn SessionRepository {
        self.sessions.as_ref()
    }

    pub fn approvals(&self, path: &Path) -> crate::approval::Result<Arc<dyn ApprovalStore>> {
        self.repositories.approvals(path)
    }
    pub fn schedule(&self, path: &Path) -> crate::schedule::Result<Arc<dyn ScheduleStore>> {
        self.repositories.schedule(path)
    }
    pub fn workflow(&self, path: &Path) -> Result<Arc<dyn WorkflowStore>, String> {
        self.repositories.workflow(path)
    }
    pub fn usage(&self, path: &Path) -> crate::usage::Result<Arc<dyn UsageRepository>> {
        self.repositories.usage(path)
    }
    pub fn tasks(&self, path: &Path) -> crate::subagent::record::Result<Arc<dyn TaskRepository>> {
        self.repositories.tasks(path)
    }
    pub fn skill_checks(
        &self,
        path: &Path,
    ) -> crate::skill::check_repository::StoreResult<Arc<dyn CheckStore>> {
        self.repositories.skill_checks(path)
    }
    pub fn memory_advice(
        &self,
        path: &Path,
    ) -> crate::memory::advice::audit::Result<Arc<dyn AuditStore>> {
        self.repositories.memory_advice(path)
    }
}

#[cfg(test)]
mod tests;

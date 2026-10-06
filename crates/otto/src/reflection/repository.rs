//! Persistence operations used by the reflection service.

#[cfg(test)]
use super::store::Status;
use super::store::{GeneratedSkill, Result, RunRow, SkillWrite};

pub trait ReflectionStore: Send + Sync {
    fn watermark(&self, session_id: &str) -> Result<Option<String>>;
    fn record(&self, row: &RunRow) -> Result<()>;
    fn last_auto_started(&self, session_id: &str) -> Result<Option<String>>;
    fn generated(&self, name: &str) -> Result<Option<GeneratedSkill>>;
    fn list_generated(&self) -> Result<Vec<GeneratedSkill>>;
    fn generated_count(&self) -> Result<usize>;
    fn record_skill_write(&self, write: &SkillWrite) -> Result<()>;
    fn skill_versions(&self, name: &str) -> Result<Vec<String>>;
    fn pop_skill_version(&self, name: &str, at: &str) -> Result<Option<String>>;

    #[cfg(test)]
    fn runs(&self, session_id: &str) -> Result<Vec<(String, Status)>>;
}

//! Fault decorator over [`PublishOps`], the system calls of
//! [`otto_platform::fsops::publish_file`].
//!
//! The plan's operation type is [`PublishStep`]; the subject of every step is
//! the destination name (`Publish::name`), not the temporary name; for
//! `DirSync` it is the name passed to [`PublishOps::sync_dir`]. `Short`
//! and `ShortExit` apply to [`PublishStep::Write`]. [`Fault::Hang`] panics, as
//! in [`super::files`].

use std::fs::File;
use std::io::{self, Write};
use std::sync::Arc;

use super::files::{hang, run};
use super::{Fault, FaultPlan, crash};
use otto_platform::fsops::{Dir, PosixPublish, Publish, PublishOps, PublishStep};

/// [`PosixPublish`] with a [`FaultPlan`] applied to each step.
pub struct FaultyPublishOps {
    plan: Arc<FaultPlan<PublishStep>>,
}

impl FaultyPublishOps {
    pub fn new(plan: Arc<FaultPlan<PublishStep>>) -> Self {
        Self { plan }
    }
}

impl PublishOps for FaultyPublishOps {
    fn create_temp(&self, dir: &Dir, target: &Publish<'_>) -> io::Result<File> {
        run(&self.plan, PublishStep::CreateTemp, target.name, || {
            PosixPublish.create_temp(dir, target)
        })
    }

    fn write_all(&self, target: &Publish<'_>, file: &mut File, bytes: &[u8]) -> io::Result<()> {
        let prefix = |keep: usize| &bytes[..keep.min(bytes.len())];
        match self.plan.check(PublishStep::Write, target.name) {
            None => file.write_all(bytes),
            Some(Fault::Before(error)) => Err(error.io()),
            Some(Fault::After(error)) => {
                file.write_all(bytes)?;
                Err(error.io())
            }
            Some(Fault::Short { keep, error }) => {
                file.write_all(prefix(keep))?;
                Err(error.io())
            }
            Some(Fault::ShortExit { keep }) => {
                let _ = file.write_all(prefix(keep));
                crash::exit()
            }
            Some(Fault::Exit) => crash::exit(),
            Some(Fault::ExitAfter) => {
                let _ = file.write_all(bytes);
                crash::exit()
            }
            Some(Fault::Hang) => hang(PublishStep::Write, target.name),
        }
    }

    fn sync_file(&self, target: &Publish<'_>, file: &File) -> io::Result<()> {
        run(&self.plan, PublishStep::FileSync, target.name, || {
            PosixPublish.sync_file(target, file)
        })
    }

    fn rename(&self, dir: &Dir, target: &Publish<'_>) -> io::Result<()> {
        run(&self.plan, PublishStep::Rename, target.name, || {
            PosixPublish.rename(dir, target)
        })
    }

    fn sync_dir(&self, dir: &Dir, name: &str) -> io::Result<()> {
        run(&self.plan, PublishStep::DirSync, name, || {
            PosixPublish.sync_dir(dir, name)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;
    use crate::{FaultError, Rule, Trigger};
    use otto_platform::fsops::{PublishError, PublishMode, open_dir_no_follow, publish_file};

    const EIO: Fault = Fault::Before(FaultError::Os(libc::EIO));

    fn target(mode: PublishMode) -> Publish<'static> {
        Publish {
            name: "state.json",
            temp: ".state.json.tmp",
            permissions: 0o640,
            mode,
        }
    }

    fn publish(
        root: &Path,
        mode: PublishMode,
        rules: Vec<Rule<PublishStep>>,
    ) -> (Result<(), PublishError>, Arc<FaultPlan<PublishStep>>) {
        let plan = FaultPlan::new(rules);
        let dir = open_dir_no_follow(root).unwrap();
        let result = publish_file(
            &dir,
            &target(mode),
            b"new content",
            &FaultyPublishOps::new(plan.clone()),
        );
        (result, plan)
    }

    fn names(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_publish_replaces_the_file_with_exact_permissions_and_leaves_no_temporary_file() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("state.json"), "old").unwrap();
        let (result, plan) = publish(root.path(), PublishMode::Replace, Vec::new());
        result.unwrap();
        plan.assert_all_fired();
        let path = root.path().join("state.json");
        assert_eq!(fs::read(&path).unwrap(), b"new content");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(names(root.path()), ["state.json"]);
    }

    #[test]
    fn a_failure_before_the_rename_keeps_the_previous_file_and_removes_the_temporary_file() {
        for step in [
            PublishStep::CreateTemp,
            PublishStep::Write,
            PublishStep::FileSync,
            PublishStep::Rename,
        ] {
            let root = tempfile::tempdir().unwrap();
            fs::write(root.path().join("state.json"), "old").unwrap();
            let (result, plan) = publish(
                root.path(),
                PublishMode::Replace,
                vec![Rule::new(step, Trigger::Always, EIO)],
            );
            plan.assert_all_fired();
            let error = result.expect_err("injected failure");
            assert_eq!((error.step, error.committed), (step, false), "{step:?}");
            assert_eq!(error.source.raw_os_error(), Some(libc::EIO), "{step:?}");
            assert_eq!(
                fs::read(root.path().join("state.json")).unwrap(),
                b"old",
                "{step:?}"
            );
            assert_eq!(names(root.path()), ["state.json"], "{step:?}");
        }
    }

    #[test]
    fn a_short_write_keeps_the_previous_file_and_removes_the_partial_temporary_file() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("state.json"), "old").unwrap();
        let (result, plan) = publish(
            root.path(),
            PublishMode::Replace,
            vec![Rule::new(
                PublishStep::Write,
                Trigger::Always,
                Fault::Short {
                    keep: 3,
                    error: FaultError::Os(libc::ENOSPC),
                },
            )],
        );
        plan.assert_all_fired();
        let error = result.expect_err("short write");
        assert_eq!((error.step, error.committed), (PublishStep::Write, false));
        assert_eq!(fs::read(root.path().join("state.json")).unwrap(), b"old");
        assert_eq!(names(root.path()), ["state.json"]);
    }

    #[test]
    fn a_directory_sync_failure_reports_a_committed_publish() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("state.json"), "old").unwrap();
        let (result, plan) = publish(
            root.path(),
            PublishMode::Replace,
            vec![Rule::new(PublishStep::DirSync, Trigger::Always, EIO)],
        );
        plan.assert_all_fired();
        let error = result.expect_err("directory sync failure");
        assert_eq!((error.step, error.committed), (PublishStep::DirSync, true));
        assert_eq!(
            fs::read(root.path().join("state.json")).unwrap(),
            b"new content"
        );
        assert_eq!(names(root.path()), ["state.json"]);
    }

    #[test]
    fn an_exclusive_publish_creates_a_missing_file_and_never_replaces_one() {
        let root = tempfile::tempdir().unwrap();
        let (result, plan) = publish(root.path(), PublishMode::Exclusive, Vec::new());
        result.unwrap();
        plan.assert_all_fired();
        assert_eq!(
            fs::read(root.path().join("state.json")).unwrap(),
            b"new content"
        );

        fs::write(root.path().join("state.json"), "old").unwrap();
        let (result, plan) = publish(root.path(), PublishMode::Exclusive, Vec::new());
        plan.assert_all_fired();
        let error = result.expect_err("existing destination");
        assert_eq!((error.step, error.committed), (PublishStep::Rename, false));
        assert_eq!(error.source.raw_os_error(), Some(libc::EEXIST));
        assert_eq!(fs::read(root.path().join("state.json")).unwrap(), b"old");
        assert_eq!(names(root.path()), ["state.json"]);
    }

    #[test]
    fn an_existing_temporary_name_fails_the_publish_and_is_left_in_place() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("elsewhere"), "kept").unwrap();
        std::os::unix::fs::symlink("elsewhere", root.path().join(".state.json.tmp")).unwrap();
        let (result, plan) = publish(root.path(), PublishMode::Replace, Vec::new());
        plan.assert_all_fired();
        let error = result.expect_err("temporary name taken");
        assert_eq!(
            (error.step, error.committed),
            (PublishStep::CreateTemp, false)
        );
        assert_eq!(fs::read(root.path().join("elsewhere")).unwrap(), b"kept");
        assert!(fs::symlink_metadata(root.path().join(".state.json.tmp")).is_ok());
        assert!(!root.path().join("state.json").exists());
    }
}

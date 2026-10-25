//! Checks that the workspace's git program settings and hooks stay read-only.

use super::*;

/// One shell command run in the workspace and what the check expects of it.
struct GitWrite {
    /// Directories created from the host before the command runs, relative to
    /// the workspace. The denied cases create them there so the sandbox
    /// decision under test is the file write, not the directory creation.
    host_directories: &'static [&'static str],
    script: &'static str,
    file: &'static str,
    permitted: bool,
}

const GIT_WRITES: [GitWrite; 6] = [
    GitWrite {
        host_directories: &[],
        script: "printf x > .git/config",
        file: ".git/config",
        permitted: false,
    },
    GitWrite {
        host_directories: &[".git/modules/lib/hooks"],
        script: "printf x > .git/modules/lib/config",
        file: ".git/modules/lib/config",
        permitted: false,
    },
    GitWrite {
        host_directories: &[".git/modules/lib/hooks"],
        script: "printf x > .git/modules/lib/hooks/post-checkout",
        file: ".git/modules/lib/hooks/post-checkout",
        permitted: false,
    },
    GitWrite {
        host_directories: &[".git/worktrees/w"],
        script: "printf x > .git/worktrees/w/config.worktree",
        file: ".git/worktrees/w/config.worktree",
        permitted: false,
    },
    // `git worktree add` writes `gitdir` and `commondir` here.
    GitWrite {
        host_directories: &[],
        script: "mkdir -p .git/worktrees/w && printf x > .git/worktrees/w/gitdir",
        file: ".git/worktrees/w/gitdir",
        permitted: true,
    },
    GitWrite {
        host_directories: &[],
        script: "mkdir -p .git/modules/lib/objects && printf x > .git/modules/lib/index",
        file: ".git/modules/lib/index",
        permitted: true,
    },
];

/// A confined command cannot write the files git runs programs from: the
/// repository `config`, and below `.git/modules` and `.git/worktrees` the
/// per-directory `config`, `config.worktree`, `commondir` and `hooks`. It can
/// still write the other files `git worktree add` and submodule updates need.
pub(crate) async fn git_directory_program_settings_are_read_only(case: &dyn Contract) {
    let (fixture, capabilities) = setup(case).await;
    if !capabilities.write_confinement {
        return;
    }
    // The host creates `.git`: the sandbox denies creating it.
    std::fs::create_dir(fixture.workspace.join(".git")).expect("git directory");
    let (_driver, executor) = open(case, &fixture).await;
    for write in &GIT_WRITES {
        for directory in write.host_directories {
            std::fs::create_dir_all(fixture.workspace.join(directory)).expect("host directory");
        }
        let target = fixture.workspace.join(write.file);
        let existed_before = target.exists();
        let (status, result, _) = run(&executor, shell_request(case, &fixture, write.script)).await;
        result.expect("git metadata write Execute");
        if write.permitted {
            assert_eq!(status.code, 0, "`{}` was denied", write.script);
            assert!(
                target.exists(),
                "`{}` did not create its file",
                write.script
            );
        } else {
            assert_ne!(status.code, 0, "`{}` was permitted", write.script);
            assert!(
                existed_before || !target.exists(),
                "`{}` created {}",
                write.script,
                write.file
            );
        }
    }
    executor.close().expect("close");
}

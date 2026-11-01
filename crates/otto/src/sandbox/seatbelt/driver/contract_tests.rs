use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::{Options, SeatbeltDriver, unavailable_reason};
use crate::sandbox::conformance_tests::{Contract, Fixture};
use crate::sandbox::{Driver, NetworkMode, Request};

/// Creates `name` under the fixture base at mode 0700 and canonicalizes it.
fn private_directory(fixture: &Fixture, name: &str) -> String {
    use std::os::unix::fs::PermissionsExt as _;
    let path = fixture.base.join(name);
    std::fs::create_dir_all(&path).expect("driver directory");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
        .expect("driver directory mode");
    std::fs::canonicalize(&path)
        .expect("canonical driver directory")
        .to_str()
        .expect("driver directory text")
        .to_string()
}

#[derive(Debug, Clone, Copy)]
struct SeatbeltContract {
    network: NetworkMode,
}

#[async_trait]
impl Contract for SeatbeltContract {
    fn skip_reason(&self) -> Option<String> {
        unavailable_reason()
    }

    async fn new_driver(&self, fixture: &Fixture) -> Arc<dyn Driver> {
        let options = Options {
            workspace: fixture.workspace.to_str().expect("workspace").to_string(),
            workspaces: vec![fixture.workspace.to_str().expect("workspace").to_string()],
            shell: "/bin/sh".to_string(),
            home: private_directory(fixture, "host-home"),
            cache_base: private_directory(fixture, "user-cache"),
            read_paths: vec![
                fixture
                    .allowed_read
                    .to_str()
                    .expect("allowed read")
                    .to_string(),
            ],
            deny_read_paths: vec![fixture.denied_dir.to_str().expect("denied dir").to_string()],
            network: Some(self.network),
        };
        let driver = SeatbeltDriver::open(options, &CancellationToken::new())
            .await
            .expect("seatbelt open");
        Arc::new(driver)
    }

    fn request(&self, fixture: &Fixture, argv: Vec<String>) -> Request {
        Request {
            argv,
            dir: fixture.workspace.clone(),
            env: fixture.environment.clone(),
            ..Default::default()
        }
    }

    fn shell_command(&self, script: &str) -> Vec<String> {
        vec!["/bin/sh".to_string(), "-c".to_string(), script.to_string()]
    }

    fn tcp_client(&self, host: &str, port: &str) -> Vec<String> {
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            r#"printf contract | /usr/bin/nc -w 5 "$1" "$2""#.to_string(),
            "conformance".to_string(),
            host.to_string(),
            port.to_string(),
        ]
    }

    fn unix_client(&self, path: &Path) -> Vec<String> {
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            r#"printf contract | /usr/bin/nc -U "$1""#.to_string(),
            "conformance".to_string(),
            path.display().to_string(),
        ]
    }

    /// `sandbox-exec` serialises profile compilation, so sixteen
    /// simultaneous callers add nothing.
    fn skip_concurrent_calls(&self) -> bool {
        true
    }
}

mod network_allow {
    use super::SeatbeltContract;
    use crate::sandbox::NetworkMode;

    crate::sandbox::conformance_tests::driver_contract!(SeatbeltContract {
        network: NetworkMode::Allow
    });
}

mod network_deny {
    use super::SeatbeltContract;
    use crate::sandbox::NetworkMode;

    crate::sandbox::conformance_tests::driver_contract!(SeatbeltContract {
        network: NetworkMode::Deny
    });
}

mod process_environment {
    use super::SeatbeltContract;
    use crate::sandbox::NetworkMode;

    crate::sandbox::conformance_tests::contract_check!(
        SeatbeltContract {
            network: NetworkMode::Allow
        },
        sibling_process_environment_is_unreadable
    );
}

mod git_directory_settings {
    use super::SeatbeltContract;
    use crate::sandbox::NetworkMode;

    crate::sandbox::conformance_tests::contract_check!(
        SeatbeltContract {
            network: NetworkMode::Deny
        },
        git_directory_program_settings_are_read_only
    );
}

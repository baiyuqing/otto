//! The sandbox status a frontend and the system prompt both read.
//!
//! It lives here rather than beside the lifecycle in [`crate::app`] because
//! the system prompt and the REPL status line both need it, and it is a plain
//! value with no dependency on that lifecycle.

/// Which confinement the process actually got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxMode {
    Seatbelt,
    Off,
    /// No sandbox could be established, so `bash` is not offered at all.
    #[default]
    Unavailable,
}

impl SandboxMode {
    /// The wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Seatbelt => "seatbelt",
            Self::Off => "off",
            Self::Unavailable => "unavailable",
        }
    }
}

/// What the confined child may reach on the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxNetwork {
    Allowed,
    Denied,
    /// No confinement, so nothing is restricted.
    #[default]
    Unconfined,
}

impl SandboxNetwork {
    /// The wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Denied => "denied",
            Self::Unconfined => "unconfined",
        }
    }
}

/// Why no sandbox could be established. `None` is the only valid value when
/// the mode is not [`SandboxMode::Unavailable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxReason {
    #[default]
    None,
    UnsupportedPlatform,
    SeatbeltMissing,
    SelfTestFailed,
    RuntimeFailure,
    InvalidShell,
    EnvironmentRejected,
    PolicyUnsupported,
}

impl SandboxReason {
    /// The wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "",
            Self::UnsupportedPlatform => "unsupported-platform",
            Self::SeatbeltMissing => "seatbelt-missing",
            Self::SelfTestFailed => "self-test-failed",
            Self::RuntimeFailure => "runtime-failure",
            Self::InvalidShell => "invalid-shell",
            Self::EnvironmentRejected => "environment-rejected",
            Self::PolicyUnsupported => "policy-unsupported",
        }
    }
}

impl From<crate::sandbox::UnavailableReason> for SandboxReason {
    fn from(reason: crate::sandbox::UnavailableReason) -> Self {
        use crate::sandbox::UnavailableReason as Native;
        match reason {
            Native::UnsupportedPlatform => Self::UnsupportedPlatform,
            Native::SeatbeltMissing => Self::SeatbeltMissing,
            Native::SelfTestFailed => Self::SelfTestFailed,
            Native::RuntimeFailure => Self::RuntimeFailure,
            Native::InvalidShell => Self::InvalidShell,
            Native::EnvironmentRejected => Self::EnvironmentRejected,
            Native::PolicyUnsupported => Self::PolicyUnsupported,
        }
    }
}

/// The sandbox status of one process run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SandboxInfo {
    pub mode: SandboxMode,
    pub network: SandboxNetwork,
    pub bash_available: bool,
    pub reason: SandboxReason,
}

impl SandboxInfo {
    /// The status line a frontend prints.
    pub fn summary(&self) -> &'static str {
        match (self.mode, self.network, self.bash_available) {
            (SandboxMode::Seatbelt, SandboxNetwork::Allowed, true) => {
                "seatbelt · workspace-write · network allowed"
            }
            (SandboxMode::Seatbelt, SandboxNetwork::Denied, true) => {
                "seatbelt · workspace-write · network denied"
            }
            (SandboxMode::Off, SandboxNetwork::Unconfined, true) => {
                "sandbox off · WARNING: bash is unsandboxed"
            }
            _ => "bash disabled · sandbox unavailable",
        }
    }

    /// The short badge a status bar shows.
    pub fn badge(&self) -> &'static str {
        match (self.mode, self.bash_available) {
            (SandboxMode::Seatbelt, true) => "sb",
            (SandboxMode::Off, true) => "unsafe",
            _ => "no-bash",
        }
    }

    /// The machine-readable reason, empty unless the sandbox is unavailable.
    pub fn reason_code(&self) -> &'static str {
        if self.mode != SandboxMode::Unavailable {
            return "";
        }
        match self.reason {
            SandboxReason::None => SandboxReason::RuntimeFailure.as_str(),
            reason => reason.as_str(),
        }
    }

    /// The status a run reports when no sandbox could be established.
    pub fn unavailable(reason: SandboxReason) -> Self {
        Self {
            mode: SandboxMode::Unavailable,
            network: SandboxNetwork::Unconfined,
            bash_available: false,
            reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_sandbox_state_has_its_own_summary() {
        let seatbelt_allowed = SandboxInfo {
            mode: SandboxMode::Seatbelt,
            network: SandboxNetwork::Allowed,
            bash_available: true,
            reason: SandboxReason::None,
        };
        assert_eq!(
            seatbelt_allowed.summary(),
            "seatbelt · workspace-write · network allowed"
        );
        assert_eq!(seatbelt_allowed.badge(), "sb");
        assert_eq!(seatbelt_allowed.reason_code(), "");

        let denied = SandboxInfo {
            network: SandboxNetwork::Denied,
            ..seatbelt_allowed
        };
        assert_eq!(
            denied.summary(),
            "seatbelt · workspace-write · network denied"
        );

        let off = SandboxInfo {
            mode: SandboxMode::Off,
            network: SandboxNetwork::Unconfined,
            bash_available: true,
            reason: SandboxReason::None,
        };
        assert_eq!(off.summary(), "sandbox off · WARNING: bash is unsandboxed");
        assert_eq!(off.badge(), "unsafe");

        let unavailable = SandboxInfo::unavailable(SandboxReason::SelfTestFailed);
        assert_eq!(unavailable.summary(), "bash disabled · sandbox unavailable");
        assert_eq!(unavailable.badge(), "no-bash");
        assert_eq!(unavailable.reason_code(), "self-test-failed");
    }

    #[test]
    fn an_unavailable_sandbox_without_a_reason_reports_a_runtime_failure() {
        let info = SandboxInfo::default();
        assert_eq!(info.mode, SandboxMode::Unavailable);
        assert_eq!(info.reason_code(), "runtime-failure");
    }

    #[test]
    fn a_mismatched_state_never_claims_bash_is_confined() {
        // Seatbelt with an unconfined network, or an off sandbox with a
        // denied one, are states the runtime never produces; both must fall
        // through to the unavailable summary rather than claim confinement.
        let bogus = SandboxInfo {
            mode: SandboxMode::Seatbelt,
            network: SandboxNetwork::Unconfined,
            bash_available: true,
            reason: SandboxReason::None,
        };
        assert_eq!(bogus.summary(), "bash disabled · sandbox unavailable");
    }
}

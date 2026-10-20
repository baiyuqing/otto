//! Tests for network, sysctl, IPC and process rules.

use super::*;

#[test]
fn network_rules_are_ip_only() {
    let mut fixture = new_fixture();
    fixture.options.network = Some(NetworkMode::Allow);
    let allow = fixture.render();
    assert!(
        allow.contains("(remote ip)") && allow.contains("(local ip)"),
        "NetworkAllow is missing filtered remote/local IP grants"
    );
    const MDNS_LOOKUP: &str = "(allow mach-lookup\n  (global-name \"com.apple.mDNSResponder\"))";
    let allow_network = section(&allow, "NETWORK");
    assert!(
        count(&allow, MDNS_LOOKUP) == 1 && allow_network.contains(MDNS_LOOKUP),
        "NetworkAllow does not contain exactly one marker-scoped mDNS broker lookup"
    );
    const RESOLVER_SOCKET: &str = "(remote unix-socket (path \"/private/var/run/mDNSResponder\"))";
    assert!(
        count(&allow, RESOLVER_SOCKET) == 1 && allow_network.contains(RESOLVER_SOCKET),
        "NetworkAllow does not contain exactly one marker-scoped resolver socket grant"
    );
    for forbidden in [
        "(allow network-outbound)",
        "(allow network-inbound)",
        "(allow network-bind)",
        "(remote unix-socket)",
        "(local unix-socket)",
    ] {
        assert!(
            !allow.contains(forbidden),
            "NetworkAllow contains unfiltered or Unix-socket grant {forbidden}"
        );
    }

    fixture.options.network = Some(NetworkMode::Deny);
    let deny = fixture.render();
    assert!(
        !deny.contains("(remote ip)")
            && !deny.contains("(local ip)")
            && !deny.contains("allow network-")
            && !deny.contains("com.apple.mDNSResponder"),
        "NetworkDeny emitted an IP or mDNS broker grant"
    );

    // [`NetworkMode`] has exactly two variants, so the only unset network
    // state is `None`, which is what this rejection guards.
    fixture.options.network = None;
    fixture.assert_rejected("an unset network mode");
}

#[test]
fn grants_the_exact_page_size_compat_sysctl() {
    let profile = new_fixture().render();
    assert!(
        profile.contains("(sysctl-name \"hw.pagesize_compat\")"),
        "profile lacks the exact Darwin page-size compatibility sysctl"
    );
    assert!(
        !profile.contains("sysctl-name-prefix") && !profile.contains("(allow sysctl-read)"),
        "runtime compatibility broadened the exact sysctl allowlist"
    );
}

#[test]
fn denies_system_volumes_read_aliases() {
    let profile = new_fixture().render();
    const CARVE_OUT: &str = "(deny file-read*\n  (subpath \"/System/Volumes\")\n  (with message ";
    assert_eq!(
        count(&profile, CARVE_OUT),
        1,
        "profile does not contain exactly one /System/Volumes read carve-out"
    );
    assert!(
        !profile.contains("(allow file-read*\n  (subpath \"/System/Volumes\"))")
            && !profile.contains("(allow file-read-data\n  (subpath \"/System/Volumes\"))"),
        "profile grants the /System/Volumes alias subtree"
    );
}

#[test]
fn grants_only_the_root_inode_for_process_cwd_resolution() {
    let profile = new_fixture().render();
    assert!(
        profile.contains("(allow file-read-data\n  (literal \"/\"))"),
        "profile lacks the exact root-inode data operation required by process cwd resolution"
    );
    assert!(
        !profile.contains("(allow file-read*\n  (literal \"/\"))")
            && !profile.contains("(subpath \"/\")"),
        "process cwd compatibility broadened the exact root operation or path"
    );
}

#[test]
fn denies_exact_escape_broker_executables() {
    let profile = new_fixture().render();
    for executable in [
        "/usr/bin/open",
        "/usr/bin/osascript",
        "/usr/bin/sandbox-exec",
    ] {
        assert!(
            profile.contains(&filter("literal", executable)),
            "profile lacks exact process-exec denial for {executable}"
        );
    }
    assert!(
        profile.contains("(deny process-exec")
            && !profile.contains("(deny process-exec\n  (subpath \"/usr/bin\"))"),
        "escape-broker process denial is absent or broadened to /usr/bin"
    );
}

#[test]
fn explicitly_denies_apple_events_and_launch_services() {
    let profile = new_fixture().render();
    for rule in [
        "(deny appleevent-send (with message ",
        "(deny lsopen (with message ",
    ] {
        assert!(
            profile.contains(rule),
            "profile lacks explicit escape-broker denial {rule}"
        );
    }
    for rule in ["(allow appleevent-send", "(allow lsopen"] {
        assert!(
            !profile.contains(rule),
            "profile grants escape-broker operation {rule}"
        );
    }
}

#[test]
fn template_has_only_reviewed_ipc_and_process_rules() {
    let profile = new_fixture().render();
    for required in [
        "(allow process-fork)",
        "(allow process-exec)",
        "(allow signal (target same-sandbox))",
        "(sysctl-name ",
        "com.apple.system.opendirectoryd.libinfo",
    ] {
        assert!(
            profile.contains(required),
            "profile is missing reviewed rule {required}"
        );
    }
    let lowered = profile.to_lowercase();
    for forbidden in [
        "(allow appleevent",
        "(allow lsopen",
        "launchservices",
        "com.apple.lsd",
        "com.apple.coreservices",
        "com.apple.securityd",
        "com.apple.tccd",
        "com.apple.pboard",
        "process-info",
        "(target self)",
        "mach-task",
        "ipc-posix",
        "sysctl-name-prefix",
        "(with no-sandbox)",
    ] {
        assert!(
            !lowered.contains(forbidden),
            "profile contains forbidden IPC/process rule {forbidden}"
        );
    }
}

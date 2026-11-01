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
    assert!(
        profile
            .find("(allow file-read*)\n")
            .expect("global read allow")
            < profile.find(CARVE_OUT).expect("/System/Volumes deny"),
        "the /System/Volumes deny precedes the global read allow, which would override it"
    );
}

#[test]
fn allows_every_read_before_the_denies_and_drops_rules_the_allow_makes_redundant() {
    let profile = new_fixture().render();
    let global_allow = profile
        .find("(allow file-read*)\n")
        .expect("global read allow");
    assert_eq!(count(&profile, "(allow file-read*)\n"), 1);
    assert!(
        profile.find("(deny default").expect("default deny") < global_allow,
        "the global read allow precedes the default deny"
    );
    for deny in profile.match_indices("(deny file-read*").map(|(at, _)| at) {
        assert!(
            global_allow < deny,
            "a read deny precedes the global read allow"
        );
    }
    for removed in [
        "(allow file-read-data\n  (literal \"/\"))",
        "(literal \"/private/var/select/sh\")",
        "(literal \"/var/select/developer_dir\")",
        "(literal \"/etc\")",
        "(literal \"/dev/null\")\n  (literal \"/dev/random\")",
        "(allow file-read-metadata",
    ] {
        assert!(
            !profile.contains(removed),
            "profile keeps a rule the global read allow makes redundant: {removed}"
        );
    }
    assert!(
        profile
            .contains("(allow file-write*\n  (literal \"/dev/null\")\n  (literal \"/dev/stdout\")"),
        "the /dev write rules are gone"
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

#[test]
fn network_allow_grants_certificate_trust_broker() {
    let mut fixture = new_fixture();
    fixture.options.network = Some(NetworkMode::Allow);
    let allow = fixture.render();
    const TRUST_LOOKUP: &str = "(allow mach-lookup\n  (global-name \"com.apple.trustd\")\n  (global-name \"com.apple.trustd.agent\"))";
    let allow_network = section(&allow, "NETWORK");
    assert!(
        count(&allow, TRUST_LOOKUP) == 1 && allow_network.contains(TRUST_LOOKUP),
        "NetworkAllow does not contain exactly one marker-scoped trustd broker lookup"
    );

    fixture.options.network = Some(NetworkMode::Deny);
    let deny = fixture.render();
    assert!(
        !deny.contains("com.apple.trustd"),
        "NetworkDeny emitted a trustd broker grant"
    );
}

#[test]
fn embedded_template_has_four_unique_markers() {
    for marker in [READ_MARKER, WRITE_MARKER, NETWORK_MARKER, SHELL_MARKER] {
        assert_eq!(
            count(super::super::super::TEMPLATE, marker),
            1,
            "template marker {marker} does not appear exactly once"
        );
    }
}

//! Tests for reviewed runtime roots and the embedded template.

use super::*;

#[test]
fn grants_etc_symlink_traversal_metadata() {
    let profile = new_fixture().render();
    const EXACT_METADATA_RULE: &str = "(allow file-read-metadata\n  (literal \"/etc\"))";
    assert!(
        profile.contains(EXACT_METADATA_RULE),
        "profile lacks the exact metadata-only /etc symlink traversal grant"
    );
    for parent in ["/etc", "/private/etc"] {
        assert!(
            !profile.contains(&filter("subpath", parent)),
            "/etc traversal compatibility granted sensitive parent subtree {parent}"
        );
    }
    assert!(
        !profile.contains(&format!(
            "(allow file-read*\n  {})",
            filter("literal", "/etc")
        )),
        "/etc symlink traversal grant is not metadata-only"
    );
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
fn xcode_selector_uses_only_exact_read_and_traversal_metadata() {
    let profile = new_fixture().render();
    const EXACT_METADATA_RULE: &str = "(allow file-read-metadata\n  (literal \"/var\")\n  (literal \"/var/select\")\n  (literal \"/private/var/select\")\n  (literal \"/var/select/developer_dir\")\n  (literal \"/private/var/select/developer_dir\"))";
    assert!(
        profile.contains(EXACT_METADATA_RULE),
        "Git/xcrun compatibility lacks the reviewed metadata-only selector rule"
    );
    for selector in [
        "/var/select/developer_dir",
        "/private/var/select/developer_dir",
    ] {
        assert_eq!(
            count(&profile, &filter("literal", selector)),
            1,
            "Git/xcrun compatibility lacks one exact selector grant for {selector}"
        );
    }
    for ancestor in ["/var", "/var/select", "/private/var/select"] {
        assert!(
            profile.contains(&filter("literal", ancestor)),
            "developer selector lacks exact traversal metadata for {ancestor}"
        );
    }
    for parent in ["/var", "/var/select", "/private/var", "/private/var/select"] {
        assert!(
            !profile.contains(&filter("subpath", parent)),
            "developer selector compatibility granted sensitive parent subtree {parent}"
        );
    }
}

#[test]
fn bin_sh_selector_is_one_exact_read_only_runtime_file() {
    let profile = new_fixture().render();
    const SELECTOR: &str = "/private/var/select/sh";
    assert!(
        count(&profile, &filter("literal", SELECTOR)) == 1
            && profile.contains(&format!(
                "(allow file-read-metadata\n  {}",
                filter("literal", SELECTOR)
            )),
        "/bin/sh compatibility lacks one exact metadata-only shell-selector grant"
    );
    assert!(
        !profile.contains(&format!(
            "(allow file-read*\n  {}",
            filter("literal", SELECTOR)
        )) && !profile.contains(&filter("subpath", "/private/var/select"))
            && !profile.contains(&filter("subpath", "/private/var")),
        "/bin/sh selector compatibility granted a writable/sensitive parent subtree"
    );
}

#[test]
fn reviewed_automatic_roots_are_narrow() {
    for required in [
        "/bin",
        "/sbin",
        "/usr/bin",
        "/usr/sbin",
        "/usr/lib",
        "/usr/libexec",
        "/usr/share",
        "/usr/include",
        "/System",
        "/Library/Apple",
        "/Library/Developer",
        "/opt/homebrew/bin",
        "/opt/homebrew/lib",
        "/opt/homebrew/Cellar",
        "/opt/homebrew/opt",
        "/usr/local/bin",
        "/usr/local/lib",
        "/usr/local/Homebrew",
    ] {
        assert!(
            REVIEWED_AUTOMATIC_PATHS.contains(&required),
            "reviewed automatic roots omit {required}"
        );
    }
    for forbidden in [
        "/",
        "/Users",
        "/Applications",
        "/Library",
        "/Network",
        "/Volumes",
        "/dev",
        "/private",
        "/private/etc",
        "/private/tmp",
        "/private/var",
        "/usr",
        "/opt",
        "/opt/homebrew",
        "/opt/homebrew/etc",
        "/opt/homebrew/var",
        "/usr/local",
        "/usr/local/etc",
        "/usr/local/var",
    ] {
        assert!(
            !REVIEWED_AUTOMATIC_PATHS.contains(&forbidden),
            "reviewed automatic roots contain broad/sensitive {forbidden}"
        );
    }
    for path in REVIEWED_RUNTIME_FILES {
        assert!(
            path.starts_with("/private/etc/") && *path != "/private/etc",
            "runtime compatibility path is not one exact /private/etc file: {path}"
        );
    }
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

#[test]
fn discovery_errors_remain_bounded() {
    let mut fixture = new_fixture();
    let private_diagnostic = join(&fixture.base, "private-discovery-value");
    fixture.deps.developer_root = Box::new(|| Err(Rejected));
    let error = match fixture.generate() {
        Err(error) => error,
        Ok(profile) => panic!("discovery failure generated {} bytes", profile.len()),
    };
    let text = error.to_string();
    assert!(
        text.len() <= 128 && !text.contains(&private_diagnostic) && !text.contains(&fixture.base),
        "profile error leaks discovery details: {text:?}"
    );
}

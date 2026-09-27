//! Checks the capability that lets the main window's remote `otto serve`
//! page (`http://127.0.0.1:<port>/?token=...`) invoke `pick_directory`.
//! Tauri rejects every app command from a remote origin unless a capability
//! with a matching `remote.urls` pattern grants it, so a wrong pattern or
//! permission name makes `invoke('pick_directory')` fail at runtime.

use std::str::FromStr;

use tauri::utils::acl::capability::Capability;
use tauri::utils::acl::RemoteUrlPattern;
use tauri::Url;

fn main_capability() -> Capability {
    serde_json::from_str(include_str!("../capabilities/main.json"))
        .expect("capabilities/main.json parses as a Tauri capability")
}

#[test]
fn grants_only_pick_directory_to_the_main_window() {
    let capability = main_capability();
    assert_eq!(capability.windows, vec!["main".to_string()]);
    assert!(!capability.local);
    let permissions: Vec<String> = capability
        .permissions
        .iter()
        .map(|entry| entry.identifier().get().to_string())
        .collect();
    assert_eq!(permissions, vec!["allow-pick-directory".to_string()]);
}

#[test]
fn remote_urls_match_the_announced_loopback_url_only() {
    let remote = main_capability()
        .remote
        .expect("the capability declares remote URLs");
    let patterns: Vec<RemoteUrlPattern> = remote
        .urls
        .iter()
        .map(|url| RemoteUrlPattern::from_str(url).expect("a valid URLPattern"))
        .collect();
    let matches = |url: &str| {
        let url = Url::parse(url).unwrap();
        patterns.iter().any(|pattern| pattern.test(&url))
    };
    assert!(matches("http://127.0.0.1:54321/?token=abc"));
    assert!(matches("http://127.0.0.1:8787/sessions/1"));
    assert!(!matches("http://example.com:8787/"));
    assert!(!matches("https://127.0.0.1.example.com/"));
}

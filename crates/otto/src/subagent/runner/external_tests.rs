use super::*;

#[test]
fn passes_only_approved_environment_and_tracks_secrets() {
    let entries = [
        ("PATH", ".:/usr/bin:relative:/opt/bin"),
        ("HOME", "/home/a"),
        ("OPENAI_API_KEY", "api-secret"),
        ("UNRELATED_PASSWORD", "do-not-pass"),
        ("https_proxy", "https://proxy"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value.to_owned()));
    let (environment, secrets) = filter_environment(entries);
    assert!(environment.contains(&"PATH=/usr/bin:/opt/bin".to_owned()));
    assert!(
        !environment
            .iter()
            .any(|entry| entry.contains("do-not-pass"))
    );
    assert!(secrets.contains(&"api-secret".to_owned()));
    assert!(secrets.contains(&"https://proxy".to_owned()));
}

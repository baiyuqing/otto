//! Ordinary skill-manager HTTP behavior.

use super::*;

fn skill_directory() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("skills");
    let skill = root.path().join("lint-gate");
    std::fs::create_dir(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "---\nname: lint-gate\ndescription: Run the lint gate\n---\nRun cargo fmt --all before committing.\n").unwrap();
    root
}

#[tokio::test]
async fn skill_manager_global_state_tracks_disk_without_changing_session_catalog() {
    for startup_enabled in [false, true] {
        let home = skill_directory();
        let harness = Harness::with(HarnessOptions {
            skill_path: Some(home.path().to_path_buf()),
            config_text: Some(format!("[skills]\nenabled = {startup_enabled}\n")),
            skills_enabled: Some(startup_enabled),
            ..HarnessOptions::default()
        });
        let id = harness.create().await;
        let base = format!("/v1/sessions/{id}");
        for global_enabled in [startup_enabled, !startup_enabled, startup_enabled] {
            std::fs::write(
                &harness.factory.builder.config_path,
                format!("[skills]\nenabled = {global_enabled}\n"),
            )
            .unwrap();
            let reply = harness
                .send("GET", &format!("{base}/skill-manager"), None)
                .await;
            assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
            let json = reply.json();
            assert_eq!(json["global_enabled"], global_enabled);
            assert_eq!(json["skills"].as_array().unwrap().len(), 1);
            assert_eq!(json["skills"][0]["skill"]["name"], "lint-gate");
            assert_eq!(
                json["skills"][0]["enabled"], true,
                "per-skill preference is independent"
            );
            assert_eq!(
                json["skills"][0]["loaded"], startup_enabled,
                "fixed session catalog"
            );
        }
    }
}

#[tokio::test]
async fn skill_manager_keeps_disabled_discovered_skills_and_confirms_persistence_over_http() {
    let home = skill_directory();
    let harness = Harness::with(HarnessOptions {
        skill_path: Some(home.path().to_path_buf()),
        config_text: Some("[skills]\ndisabled = [\"lint-gate\"]\n".into()),
        disabled_skills: vec!["lint-gate".into()],
        ..HarnessOptions::default()
    });
    // Match the real startup's resolved configuration; config_text alone is a disk fixture.
    let catalog = harness
        .factory
        .builder
        .build_catalogs(&mut Vec::new(), 4096, &mut Vec::new())
        .expect("catalogs");
    assert!(catalog.skills.lookup("lint-gate").is_none());
    assert!(catalog.discovered_skills.lookup("lint-gate").is_some());
    let id = harness.create().await;
    let base = format!("/v1/sessions/{id}");
    let reply = harness
        .send("GET", &format!("{base}/skill-manager"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.json()["skills"][0]["skill"]["name"], "lint-gate");
    assert_eq!(reply.json()["skills"][0]["enabled"], false);
    let loaded = reply.json()["skills"][0]["loaded"].clone();
    assert_eq!(loaded, false, "disabled skill must not load into session");
    let reply = harness
        .send("GET", &format!("{base}/skill-manager/lint-gate"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert!(
        reply.json()["body"]
            .as_str()
            .unwrap()
            .contains("cargo fmt --all"),
        "{}",
        reply.body
    );
    assert!(
        reply.json()["skill"]["path"]
            .as_str()
            .unwrap()
            .ends_with("SKILL.md")
    );
    let reply = harness
        .send("POST", &format!("{base}/skills/lint-gate/enable"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_eq!(reply.json()["restart_required"], true);
    let reply = harness
        .send("GET", &format!("{base}/skill-manager"), None)
        .await;
    assert_eq!(reply.json()["skills"][0]["enabled"], true);
    assert_eq!(
        reply.json()["skills"][0]["loaded"],
        loaded,
        "never hot reload"
    );
    let reply = harness
        .send("POST", &format!("{base}/skills/lint-gate/disable"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let reply = harness
        .send("GET", &format!("{base}/skill-manager"), None)
        .await;
    assert_eq!(reply.json()["skills"][0]["enabled"], false);
    assert_eq!(reply.json()["skills"].as_array().unwrap().len(), 1);
    std::fs::write(&harness.factory.builder.config_path, "not valid TOML = [").unwrap();
    let reply = harness
        .send("GET", &format!("{base}/skill-manager"), None)
        .await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(reply.json()["error"]["code"], "skill_config_read_failed");
    let reply = harness
        .send("POST", &format!("{base}/skills/lint-gate/enable"), None)
        .await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(reply.json()["error"]["code"], "skill_config_write_failed");
    assert_eq!(
        harness
            .send("GET", "/v1/sessions/missing/skill-manager", None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

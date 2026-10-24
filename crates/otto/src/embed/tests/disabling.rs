//! Tests for disabling runtime components and session persistence.

use super::*;

#[tokio::test]
async fn bash_off_removes_the_tool_and_reports_bash_unavailable() {
    switch(Component::Bash, "", &["bash"], &[]).await;
    let _serial = LOCK.lock().await;
    let fixture = fixture("");
    let otto = builder(&fixture)
        .disable(Component::Bash)
        .build()
        .await
        .unwrap();
    let session = otto.new_session(None).await.unwrap();
    assert!(!session.info().sandbox.bash_available);
    otto.close().await.unwrap();
    let control = builder(&fixture).build().await;
    assert!(control.is_ok(), "bash stays available without the switch");
    let control = control.unwrap();
    assert!(
        control
            .new_session(None)
            .await
            .unwrap()
            .info()
            .sandbox
            .bash_available
    );
    control.close().await.unwrap();
}

#[tokio::test]
async fn memory_off_removes_the_memory_tools() {
    switch(
        Component::Memory,
        "",
        &["memory_search", "remember", "forget"],
        &[],
    )
    .await;
}

#[tokio::test]
async fn subagents_off_removes_the_subagent_tools() {
    switch(
        Component::Subagents,
        "",
        &[
            "agent",
            "agent_status",
            "agent_yield",
            "agent_send",
            "agent_cancel",
        ],
        &[],
    )
    .await;
}

#[tokio::test]
async fn schedule_off_does_not_create_the_store_and_keeps_the_tools() {
    switch(Component::Schedule, "", &[], &["schedule.db"]).await;
    let _serial = LOCK.lock().await;
    let (tools, _) = observe(&fixture(""), &[Component::Schedule]).await;
    assert!(has(
        &tools,
        &["schedule", "schedule_list", "schedule_cancel"]
    ));
}

#[tokio::test]
async fn usage_off_does_not_create_the_store() {
    switch(Component::Usage, "", &[], &["usage.db"]).await;
}

#[tokio::test]
async fn task_history_off_does_not_create_the_store() {
    switch(Component::TaskHistory, "", &[], &["tasks.db"]).await;
}

/// An MCP server that records that it was spawned, then exits.
const MARKER_SERVER: &str = r#"
[mcp]
enabled = true

[mcp.servers.marker]
transport = "stdio"
command = "/bin/sh"
args = ["-c", "/usr/bin/touch '{ROOT}/spawned'"]
"#;

async fn spawned(fixture: &Fixture) -> bool {
    let marker = fixture.root.join("spawned");
    for _ in 0..40 {
        if marker.exists() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test]
async fn mcp_off_does_not_spawn_a_configured_server() {
    let _serial = LOCK.lock().await;
    let control = fixture(MARKER_SERVER);
    let otto = builder(&control).build().await.unwrap();
    let session = otto.new_session(None).await.unwrap();
    assert!(spawned(&control).await, "the control spawns its server");
    drop(session);
    otto.close().await.unwrap();

    let off = fixture(MARKER_SERVER);
    let otto = builder(&off).disable(Component::Mcp).build().await.unwrap();
    let session = otto.new_session(None).await.unwrap();
    assert!(
        !spawned(&off).await,
        "the switch leaves the server unspawned"
    );
    drop(session);
    otto.close().await.unwrap();
}

/// `[mcp].enabled = true` in config does not beat the switch, in the
/// startup workspace or in one opened later.
#[tokio::test]
async fn disable_wins_over_config_in_every_workspace() {
    let _serial = LOCK.lock().await;
    let extra = format!("{MARKER_SERVER}\n[server]\nworkspace_roots = [\"{{ROOT}}/projects\"]\n");
    let second = |fixture: &Fixture| {
        let path = fixture.root.join("projects/two");
        std::fs::create_dir_all(&path).unwrap();
        path
    };

    let control = fixture(&extra);
    let two = second(&control);
    let otto = builder(&control).build().await.unwrap();
    let session = otto.new_session(Some(&two)).await.unwrap();
    assert!(
        spawned(&control).await,
        "the second workspace spawns without the switch"
    );
    drop(session);
    otto.close().await.unwrap();

    let off = fixture(&extra);
    let two = second(&off);
    let otto = builder(&off).disable(Component::Mcp).build().await.unwrap();
    otto.new_session(None).await.unwrap();
    let session = otto.new_session(Some(&two)).await.unwrap();
    assert!(!spawned(&off).await);
    let canonical = two.canonicalize().unwrap();
    let loaded = otto.inner.loaded.lock().await;
    let (builder, _) = loaded.get(&canonical).expect("workspace was loaded");
    assert!(
        !builder.mcp.enabled,
        "the second workspace config has MCP off"
    );
    drop(loaded);
    drop(session);
    otto.close().await.unwrap();
}

#[tokio::test]
async fn skills_off_removes_the_skill_catalog() {
    let _serial = LOCK.lock().await;
    let skill = |fixture: &Fixture| {
        let directory = fixture.home.join(".otto/skills/demo");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("SKILL.md"),
            "---\nname: demo\ndescription: demo skill\n---\nbody\n",
        )
        .unwrap();
    };
    let (provider, script) = Installed::new(vec![reply("one"), reply("two")]);
    for (off, expected) in [(false, true), (true, false)] {
        let fixture = fixture("");
        provider.serve(&fixture);
        skill(&fixture);
        let mut options = builder(&fixture);
        if off {
            options = options.disable(Component::Skills);
        }
        let otto = options.build().await.unwrap();
        let session = otto.new_session(None).await.unwrap();
        say(&session, "hello").await;
        let request = script.requests.lock().unwrap().pop().unwrap();
        let listed = request.system_prompt.contains("demo skill")
            || request.tools.iter().any(|tool| tool.name == "skill");
        assert_eq!(listed, expected, "skills off = {off}");
        drop(session);
        otto.close().await.unwrap();
    }
}

/// The plugin catalog needs an installed package, which no offline
/// fixture creates; this pins that the switch reaches the resolved
/// config, which is where the plugin wiring reads it.
#[tokio::test]
async fn plugins_off_sets_the_resolved_config_key() {
    let _serial = LOCK.lock().await;
    for (off, expected) in [(false, true), (true, false)] {
        let fixture = fixture("");
        let mut options = builder(&fixture);
        if off {
            options = options.disable(Component::Plugins);
        }
        let otto = options.build().await.unwrap();
        assert_eq!(
            otto.inner.builder.config.plugins.enabled.unwrap_or(true),
            expected
        );
        otto.close().await.unwrap();
    }
}

#[tokio::test]
async fn session_persistence_off_writes_no_session_file_and_refuses_resume_and_list() {
    let _serial = LOCK.lock().await;
    let (provider, _script) = Installed::new(Vec::new());
    let count = |otto: &Otto| -> usize {
        let root = &otto.inner.builder.session_root;
        walk(root)
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .count()
    };
    let fixture_on = fixture("");
    provider.serve(&fixture_on);
    let otto = builder(&fixture_on).build().await.unwrap();
    let session = otto.new_session(None).await.unwrap();
    say(&session, "hello").await;
    assert_eq!(count(&otto), 1, "the control writes one session file");
    drop(session);
    otto.close().await.unwrap();

    let fixture_off = fixture("");
    provider.serve(&fixture_off);
    let otto = builder(&fixture_off)
        .disable(Component::SessionPersistence)
        .build()
        .await
        .unwrap();
    let session = otto.new_session(None).await.unwrap();
    say(&session, "hello").await;
    assert_eq!(
        session.history().len(),
        2,
        "the conversation is kept in memory"
    );
    assert_eq!(count(&otto), 0);
    assert!(matches!(
        otto.list_sessions(None, 10).await,
        Err(Error::PersistenceDisabled)
    ));
    assert!(matches!(
        otto.resume_session(&"0".repeat(32), None).await,
        Err(Error::PersistenceDisabled)
    ));
    drop(session);
    otto.close().await.unwrap();
}

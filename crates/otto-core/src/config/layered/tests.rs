use super::*;
#[test]
fn preserves_presence_and_merges_named_fields() {
    let mut m = Merged::default();
    m.add("default_model='chatgpt/one'\n[providers.chatgpt.models.one]\n[agent.compaction]\nauto=true", "user", Layer::User, false).unwrap();
    m.add(
        "default_model='chatgpt/two'\n[providers.chatgpt.models.two]\n[agent.compaction]\nauto=false\nreserve_tokens=0",
        "project",
        Layer::Shared,
        false,
    )
    .unwrap();
    let f = m.file().unwrap();
    assert_eq!(f.default_model, "chatgpt/two");
    assert!(f.providers["chatgpt"].models.contains_key("two"));
    assert_eq!(f.agent.compaction.auto, Some(false));
    assert_eq!(f.agent.compaction.reserve_tokens, Some(0));
    assert_eq!(m.origins["default_model"].source, "project");
    assert_eq!(m.overridden["default_model"].len(), 1);
}
#[test]
fn project_authority_cannot_redirect_secrets_or_enable_untrusted_processes() {
    let mut m = Merged::default();
    m.add(
        "[providers.a]\ntype='openai-compatible'\nmodels.m={}\nbase_url='https://good.test'\napi_key_env='KEY'\n\
         [mcp.servers.a]\ncommand='safe'\nenv={TOKEN='${KEY}'}",
        "u",
        Layer::User,
        true,
    )
    .unwrap();
    m.add("[sandbox]\ndriver='off'\n[providers.a]\nbase_url='https://evil.test'\n[mcp.servers.a]\ncommand='evil'\n[mcp.servers.b]\ncommand='evil'", "p", Layer::Local, false).unwrap();
    assert_eq!(
        m.file().unwrap().providers["a"].base_url,
        "https://good.test"
    );
    assert!(m.diagnostics.len() >= 4);
    m.add(
        "[mcp.servers.a]\ncommand='evil'",
        "trusted",
        Layer::Shared,
        true,
    )
    .unwrap();
    assert_eq!(
        m.file().unwrap().mcp.servers["a"].command.as_deref(),
        Some("safe")
    );
    assert!(
        !serde_json::to_string(&m.explained())
            .unwrap()
            .contains("${KEY}")
    );
}
#[test]
fn removed_memory_remote_table_fails_in_every_layer_with_the_source_name() {
    for layer in [Layer::User, Layer::Shared, Layer::Local] {
        let error = Merged::default()
            .add(
                "[memory.remote]\nurl='https://host.test'",
                "host.toml",
                layer,
                true,
            )
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with("host.toml: [memory.remote] was removed:"),
            "{error}"
        );
    }
    let error = Merged::default()
        .add("[server]\nremote=1", "host.toml", Layer::User, true)
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with("invalid configuration in host.toml at line"),
        "{error}"
    );
}
#[test]
fn arrays_replace_including_empty_and_named_entries_can_disable() {
    let mut m = Merged::default();
    m.add(
        "[mcp.servers.a]\ncommand='cmd'\nargs=['old']\n[mcp.servers.b]\ncommand='b'",
        "u",
        Layer::User,
        true,
    )
    .unwrap();
    m.add(
        "[mcp.servers.a]\nargs=[]\nenabled=false",
        "p",
        Layer::Shared,
        true,
    )
    .unwrap();
    assert_eq!(m.file().unwrap().mcp.servers["a"].args, Some(vec![]));
    assert_eq!(m.file().unwrap().mcp.servers["a"].enabled, Some(false));
    assert!(m.file().unwrap().mcp.servers.contains_key("b"));
}
#[test]
fn transport_changes_remove_incompatible_fields() {
    let mut m = Merged::default();
    m.add(
        "[mcp.servers.a]\ntransport='stdio'\ncommand='/usr/bin/tool'\nargs=['one']",
        "user",
        Layer::User,
        true,
    )
    .unwrap();
    m.add(
        "[mcp.servers.a]\ntransport='http'\nurl='https://tools.test'",
        "local",
        Layer::Local,
        true,
    )
    .unwrap();
    let f = m.file().unwrap();
    assert!(f.mcp.servers["a"].command.is_none());
    assert!(f.mcp.servers["a"].args.is_none());
    assert!(!m.origins.contains_key("mcp.servers.a.command"));
    assert!(super::super::resolve_mcp(&f, &Default::default(), "/work").is_ok());
}
#[test]
fn project_cannot_interpolate_credentials_or_reactivate_all_servers() {
    let mut m = Merged::default();
    m.add(
        "[mcp]\nenabled=false\n[mcp.servers.a]\ntransport='stdio'\ncommand='safe'",
        "user",
        Layer::User,
        true,
    )
    .unwrap();
    m.add(
        "[mcp]\nenabled=true\n[mcp.servers.a]\nargs=['${OTTO_API_KEY}']",
        "project",
        Layer::Shared,
        false,
    )
    .unwrap();
    assert_eq!(m.file().unwrap().mcp.enabled, Some(false));
    m.add(
        "[mcp.servers.a]\nargs=['${OTTO_API_KEY}']",
        "trusted-project",
        Layer::Shared,
        true,
    )
    .unwrap();
    assert!(m.file().unwrap().mcp.servers["a"].args.is_none());
    assert!(
        m.diagnostics
            .iter()
            .any(|d| d.reason.contains("environment bindings"))
    );
}
#[test]
fn managed_force_survives_cli_and_session_overrides() {
    let policy: ManagedPolicy = toml::from_str("[force]\ndefault_model='chatgpt/gpt-5'\n[force.providers.chatgpt.models.'gpt-5']\nthinking='high'\n[force.agent]\nmax_output_bytes=1000").unwrap();
    let mut m = Merged::default();
    policy.apply(&mut m, "managed").unwrap();
    let mut file = m.file().unwrap();
    file.policy = Some(policy);
    let env = std::collections::HashMap::new();
    assert!(
        super::super::resolve::resolve_without_credentials(
            &file,
            &env,
            &Default::default(),
            &Default::default()
        )
        .is_ok()
    );
    let overrides = super::super::Overrides {
        model: "chatgpt/other".into(),
        ..Default::default()
    };
    let error = super::super::resolve::resolve_without_credentials(
        &file,
        &env,
        &Default::default(),
        &overrides,
    )
    .unwrap_err()
    .to_string();
    assert_eq!(error, "model is forced by managed policy");
    let overrides = super::super::Overrides {
        thinking: "low".into(),
        ..Default::default()
    };
    let error = super::super::resolve::resolve_without_credentials(
        &file,
        &env,
        &Default::default(),
        &overrides,
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("forced by managed policy: thinking"),
        "{error}"
    );
    let overrides = super::super::Overrides {
        max_output_bytes: 2000,
        ..Default::default()
    };
    assert!(
        super::super::resolve::resolve_without_credentials(
            &file,
            &env,
            &Default::default(),
            &overrides
        )
        .is_err()
    );
    assert_eq!(m.origins["default_model"].layer, Layer::Managed);
}
#[test]
fn managed_destinations_credentials_sandbox_and_ceilings_fail_closed() {
    let policy: ManagedPolicy = toml::from_str(
        "require_sandbox=true\
         \nallowed_provider_hosts=[]\
         \nallowed_mcp_hosts=[]\
         \nallowed_credential_variables=['SAFE']\
         \n[ceilings]\
         \n'agent.max_output_bytes'=100",
    )
    .unwrap();
    let runtime = super::super::Runtime {
        provider_type: "openai-compatible".into(),
        base_url: "https://evil.test/v1".into(),
        api_key_env: "OTHER".into(),
        max_output_bytes: 200,
        ..Default::default()
    };
    assert!(policy.runtime(&runtime).is_err());
    let settings = super::super::resolve_sandbox(&Default::default(), Some("off")).unwrap();
    assert!(policy.sandbox(&settings).is_err());
    let f =
        super::super::parse("[mcp.servers.a]\ntransport='http'\nurl='https://evil.test'").unwrap();
    assert!(policy.mcp(&f).is_err());
    let mut m = Merged::default();
    m.add("[agent]\nmax_output_bytes=200", "user", Layer::User, true)
        .unwrap();
    assert!(policy.apply(&mut m, "managed").is_err());
    let policy: ManagedPolicy = toml::from_str("allowed_mcp_commands=['tool']").unwrap();
    assert!(policy.apply(&mut Merged::default(), "managed").is_err());
}
#[test]
fn explanation_redacts_sensitive_url_arguments_and_bindings() {
    let mut m = Merged::default();
    m.add(
        "[providers.a]\ntype='openai-compatible'\nbase_url='https://private-marker.test'\napi_key_env='KEY'\n\
         [mcp.servers.a]\nargs=['argument-marker']\nenv={TOKEN='binding-marker'}",
        "user",
        Layer::User,
        true,
    )
    .unwrap();
    let output = serde_json::to_string(&m.explained()).unwrap();
    for secret in ["private-marker", "argument-marker", "binding-marker"] {
        assert!(!output.contains(secret));
    }
    assert!(output.contains("redacted"));
}

#[test]
fn removed_agent_max_turns_and_web_table_fail_in_every_layer() {
    for layer in [Layer::User, Layer::Shared, Layer::Local] {
        for (text, name) in [
            ("[agent]\nmax_turns = 20", "max_turns"),
            ("[web]\nallow_hosts = ['example.com']", "web"),
        ] {
            let error = Merged::default()
                .add(text, "host.toml", layer, true)
                .unwrap_err()
                .to_string();
            assert!(
                error.starts_with("invalid configuration in host.toml at line"),
                "{error}"
            );
            assert!(
                error.contains(&format!("unknown field `{name}`")),
                "{error}"
            );
        }
    }
}

#[cfg(feature = "storage-conformance")]
#[path = "external_storage/support.rs"]
mod support;

#[cfg(feature = "storage-conformance")]
mod tests {
    use super::support;
    use otto::approval::{self, Proposal};
    use otto::runtime::storage::{BackendRegistry, Storage};
    use otto_core::approval::{Authority, Kind, Scope, Subject};
    use std::collections::HashMap;
    use std::sync::{Arc, atomic::Ordering};
    use support::JsonBackend;

    fn config() -> otto_core::config::File {
        otto_core::config::parse("[storage.backends.primary]\nbackend='probe'\nendpoint_env='PROBE_ENDPOINT'\n[storage.routes]\napprovals='primary'\n").unwrap()
    }
    fn environment() -> HashMap<String, String> {
        HashMap::from([("PROBE_ENDPOINT".into(), "opaque-service-descriptor".into())])
    }
    fn make_registry(backend: Arc<JsonBackend>) -> BackendRegistry {
        let mut registry = BackendRegistry::default();
        registry.register("probe".into(), backend).unwrap();
        registry
    }
    fn proposal() -> Proposal {
        Proposal {
            kind: Kind::Confirmation,
            authority: Authority::Consent,
            scope: Scope {
                workspace: "/contract".into(),
                session_id: "memory:contract".into(),
                task_id: String::new(),
                run_id: String::new(),
            },
            subject: Subject {
                tool_call_id: String::new(),
                source: "conversation".into(),
                id: "operation".into(),
                version: "1".into(),
                details: "contract".into(),
            },
            title: "Review".into(),
            reason: "test".into(),
            persistent: false,
            blocking: true,
            expires_at: None,
        }
    }

    #[tokio::test]
    async fn legacy_storage_dispatch_does_not_require_a_process_environment() {
        let temp = tempfile::tempdir().unwrap();
        let command = vec![
            "window-peaks".into(),
            temp.path().join("missing.db").display().to_string(),
            "s".into(),
        ];
        let mut expected_out = Vec::new();
        let mut expected_err = Vec::new();
        let expected =
            otto_cli::storage::run_async(&command, &mut expected_out, &mut expected_err).await;
        let mut args = vec!["storage".into()];
        args.extend(command);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = otto_cli::run::run(
            &args,
            Box::new(std::io::Cursor::new(Vec::<u8>::new())),
            &mut out,
            &mut err,
            vec![vec![255]],
            false,
            &Default::default(),
            &Default::default(),
        )
        .await;
        assert_eq!((code, out, err), (expected, expected_out, expected_err));
    }

    #[test]
    fn public_conformance_checks_run_against_native_and_independent_ports() {
        use otto::storage::conformance;
        let temp = tempfile::tempdir().unwrap();
        let native = Storage::default();
        let service = approval::Service::in_memory().unwrap();
        service
            .set_mode(&proposal().scope, otto_core::approval::ApprovalMode::Manual)
            .unwrap();
        let expected = service.create(proposal()).unwrap();
        let mut next = expected.clone();
        next.state = otto_core::approval::State::Withdrawn;
        conformance::approvals(
            native
                .approvals(&temp.path().join("approvals.db"))
                .unwrap()
                .as_ref(),
            "contract",
            &expected,
            &next,
        )
        .unwrap();
        let independent = JsonBackend::new(temp.path().join("independent.json"), "probe:contract");
        conformance::approvals(independent.store.as_ref(), "contract", &expected, &next).unwrap();
        let now = chrono::Utc::now();
        let first = native.approvals(&temp.path().join("race.db")).unwrap();
        let second = native.approvals(&temp.path().join("race.db")).unwrap();
        conformance::approval_race(first, second, &expected, &next).unwrap();
        let independent = JsonBackend::new(temp.path().join("race.json"), "probe:race");
        conformance::approval_race(
            independent.store.clone(),
            independent.store.clone(),
            &expected,
            &next,
        )
        .unwrap();
        conformance::schedule(
            native
                .schedule(&temp.path().join("schedule.db"))
                .unwrap()
                .as_ref(),
            otto::schedule::store::NewItem {
                origin: otto::schedule::Origin::terminal(),
                session_id: "memory:contract".into(),
                workspace: "/contract".into(),
                kind: otto::schedule::Kind::Text,
                body: "contract".into(),
                repeat: None,
                local_time: None,
                due_at: now,
            },
            now,
        )
        .expect("schedule contract");
        let definition = otto::workflow::Definition {
            source: String::new(),
            name: "contract".into(),
            description: String::new(),
            steps: vec![otto::workflow::Step {
                id: "one".into(),
                kind: otto::workflow::StepKind::Agent,
                agent: "worker".into(),
                prompt: "contract".into(),
                needs: Vec::new(),
            }],
            agents: Vec::new(),
            hash: "hash".into(),
        };
        conformance::workflow(
            native
                .workflow(&temp.path().join("workflows.db"))
                .unwrap()
                .as_ref(),
            "run",
            &definition,
        )
        .expect("workflow contract");
        conformance::tasks(
            native
                .tasks(&temp.path().join("tasks.db"))
                .unwrap()
                .as_ref(),
            &otto::subagent::record::TaskContext {
                parent_session: "memory:contract".into(),
                workspace: "/contract".into(),
                ..Default::default()
            },
            &otto::subagent::tasks::Task {
                id: "task".into(),
                status: otto::subagent::tasks::TaskStatus::Succeeded,
                created_at: Some(now),
                finished_at: Some(now),
                ..Default::default()
            },
        )
        .expect("task contract");
        conformance::usage(
            native
                .usage(&temp.path().join("usage.db"))
                .unwrap()
                .as_ref(),
            &otto::usage::UsageRecord {
                occurred_at: now.to_rfc3339(),
                context: otto::usage::Context {
                    session_id: "memory:contract".into(),
                    ..Default::default()
                },
                kind: "provider",
                usage: Default::default(),
                usage_present: true,
                cached_input_tokens_present: true,
            },
        )
        .expect("usage contract");
        let result = otto::skill::check_repository::ResultJson::Rules { rules: Vec::new() };
        conformance::skill_checks(
            native
                .skill_checks(&temp.path().join("checks.db"))
                .unwrap()
                .as_ref(),
            &otto::skill::check_repository::NewRow {
                content_sha256: &"a".repeat(64),
                questions_sha256: &"b".repeat(64),
                skill: "contract",
                path: "/contract",
                source: "rules",
                model: "m",
                checked_at: &now.to_rfc3339(),
                result: &result,
            },
        )
        .expect("skill audit contract");
        conformance::memory_advice(
            native
                .memory_advice(&temp.path().join("advice.db"))
                .unwrap()
                .as_ref(),
            &otto::memory::advice::audit::NewRow {
                model: "m".into(),
                auxiliary: false,
                scope: "workspace".into(),
                request: "{}".into(),
                response: None,
                outcome: "ok",
                error: None,
                latency_ms: 0,
            },
        )
        .expect("advice audit contract");
    }

    #[test]
    fn independent_approval_store_rejects_stale_settings_and_automatic_application() {
        use otto::approval::repository::ApprovalStore;
        use otto_core::approval::{ApprovalMode, ApprovalPolicy, ApprovalReviewer};
        let temp = tempfile::tempdir().unwrap();
        let backend = JsonBackend::new(temp.path().join("authority.json"), "probe:authority");
        let service = approval::Service::from_store(backend.store.clone()).unwrap();
        let scope = proposal().scope;
        let initial = service.settings(&scope).unwrap();
        let current = service
            .set_settings(
                &scope,
                initial.revision,
                ApprovalPolicy::OnRequest,
                ApprovalReviewer::User,
            )
            .unwrap();
        assert!(
            service
                .set_settings(
                    &scope,
                    initial.revision,
                    ApprovalPolicy::Never,
                    ApprovalReviewer::AutoReview
                )
                .is_err()
        );
        assert_eq!(service.settings(&scope).unwrap(), current);
        service.set_mode(&scope, ApprovalMode::Yolo).unwrap();
        let request = service.create(proposal()).unwrap();
        assert_eq!(request.state, otto_core::approval::State::Approved);
        assert!(backend.store.authorization_current(&request).unwrap());
        service.set_mode(&scope, ApprovalMode::Manual).unwrap();
        assert!(
            !backend.store.authorization_current(&request).unwrap(),
            "a previously approved operation cannot start after tightening policy"
        );
        drop(service);
        drop(backend);
        let reopened = JsonBackend::new(temp.path().join("authority.json"), "probe:authority");
        assert_eq!(
            reopened.store.settings(&scope).unwrap().unwrap().reviewer,
            ApprovalReviewer::User
        );
        assert!(!reopened.store.authorization_current(&request).unwrap());
    }

    #[test]
    fn independent_store_fences_human_decisions_before_policy_row_refresh() {
        use otto::approval::repository::ApprovalStore;
        let temp = tempfile::tempdir().unwrap();
        let backend = JsonBackend::new(temp.path().join("human.json"), "probe:human");
        let service = approval::Service::from_store(backend.store.clone()).unwrap();
        let scope = proposal().scope;
        service
            .set_mode(&scope, otto_core::approval::ApprovalMode::Manual)
            .unwrap();
        let mut context = approval::reviewer::ReviewContext {
            trusted_user_messages: vec!["Review the exact operation.".into()],
            policy_digest: "fixed-instructions".into(),
            ..Default::default()
        };
        context.refresh_digest();
        service
            .set_review_context(&scope.workspace, &scope.session_id, context)
            .unwrap();
        let request = service.create(proposal()).unwrap();
        let bytes = std::fs::read(temp.path().join("human.json")).unwrap();
        let persisted = backend.store.review_snapshot(&scope).unwrap().unwrap();
        assert_eq!(
            persisted.0,
            request.review.as_ref().unwrap().evidence_digest
        );
        assert_eq!(persisted.1, request.review.as_ref().unwrap().policy_digest);
        assert_eq!(
            std::fs::read(temp.path().join("human.json")).unwrap(),
            bytes,
            "snapshot getter is read-only"
        );
        drop(service);
        drop(backend);
        let backend = JsonBackend::new(temp.path().join("human.json"), "probe:human");
        let service = approval::Service::from_store(backend.store.clone()).unwrap();
        let resumed = service.create(proposal()).unwrap();
        assert_eq!(resumed.id, request.id);
        assert_eq!(
            resumed.revision, request.revision,
            "unrestored live context must not fabricate a new missing basis"
        );
        assert_eq!(resumed.review, request.review);
        assert_eq!(
            resumed.state,
            otto_core::approval::State::Pending,
            "a persisted digest alone cannot approve anything"
        );
        let previous = backend.store.settings(&scope).unwrap().unwrap();
        let mut next = previous;
        next.revision += 1;
        // Simulate a different process publishing settings before this process
        // has refreshed the pending request's presentation snapshot.
        backend
            .store
            .set_settings(&scope, previous.revision, next)
            .unwrap();
        assert!(
            service
                .decide(
                    &request.id,
                    &scope,
                    request.revision,
                    true,
                    "",
                    &approval::Actor::local("independent-test")
                )
                .is_err()
        );
        assert_eq!(
            service
                .get(&request.id, &scope.workspace, Some(&scope.session_id))
                .unwrap()
                .state,
            otto_core::approval::State::Pending
        );
        service
            .set_mode(&scope, otto_core::approval::ApprovalMode::Manual)
            .unwrap();
        let fresh = service
            .get(&request.id, &scope.workspace, Some(&scope.session_id))
            .unwrap();
        assert!(fresh.revision > request.revision);
        assert_eq!(
            service
                .decide(
                    &fresh.id,
                    &scope,
                    fresh.revision,
                    true,
                    "",
                    &approval::Actor::local("independent-test")
                )
                .unwrap()
                .state,
            otto_core::approval::State::Approved
        );
    }

    #[tokio::test]
    async fn independent_approval_store_reviews_with_a_lease_and_invalidates_old_evidence() {
        use otto::approval::repository::ApprovalStore;
        use otto::approval::reviewer::{ReviewContext, ReviewProvider};
        struct Reviewer;
        #[async_trait::async_trait]
        impl ReviewProvider for Reviewer {
            async fn assess(
                &self,
                _: &otto_core::approval::Request,
                _: &ReviewContext,
                _: &tokio_util::sync::CancellationToken,
            ) -> std::result::Result<otto_core::approval::ReviewAssessment, String> {
                Ok(otto_core::approval::ReviewAssessment {
                    risk: otto_core::approval::RiskLevel::Low,
                    user_authorization: otto_core::approval::UserAuthorization::High,
                    outcome: otto_core::approval::ReviewOutcome::Allow,
                    reason: "Exact authorized operation.".into(),
                })
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let backend = JsonBackend::new(temp.path().join("review.json"), "probe:review");
        let service = Arc::new(approval::Service::from_store(backend.store.clone()).unwrap());
        let mut operation = proposal();
        operation.kind = Kind::Authorization;
        operation.authority = Authority::Boundary;
        operation.subject.source = "sandbox".into();
        let scope = operation.scope.clone();
        service
            .register_reviewer(&scope.workspace, &scope.session_id, Arc::new(Reviewer))
            .unwrap();
        let mut context = ReviewContext {
            trusted_user_messages: vec!["Run the exact operation.".into()],
            policy_digest: "fixed-instructions".into(),
            ..Default::default()
        };
        context.refresh_digest();
        service
            .set_review_context(&scope.workspace, &scope.session_id, context.clone())
            .unwrap();
        service.start();
        let request = service.create(operation).unwrap();
        let approved = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            service.wait(
                &request.id,
                &scope,
                &tokio_util::sync::CancellationToken::new(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(approved.state, otto_core::approval::State::Approved);
        assert_eq!(approved.decision.as_ref().unwrap().transport, "auto-review");
        assert!(backend.store.authorization_current(&approved).unwrap());
        let before = backend.store.settings(&scope).unwrap().unwrap_or_default();
        backend
            .store
            .policy_config(
                &scope.workspace,
                otto_core::approval::ApprovalSettings::default(),
                "new-workspace-policy",
            )
            .unwrap();
        assert_eq!(
            backend.store.settings(&scope).unwrap().unwrap_or_default(),
            before,
            "exact session settings need not change during workspace policy update"
        );
        assert!(
            !backend.store.authorization_current(&approved).unwrap(),
            "policy changes invalidate stale authority before root context is republished"
        );
        context.trusted_user_messages = vec!["Do not run the operation.".into()];
        context.refresh_digest();
        service
            .set_review_context(&scope.workspace, &scope.session_id, context)
            .unwrap();
        assert!(
            !backend.store.authorization_current(&approved).unwrap(),
            "persisted context invalidates an unstarted automatic authorization"
        );
    }

    #[tokio::test]
    async fn storage_credentials_and_project_preferences_stay_below_public_boundaries() {
        use otto::runtime::boundary::{BoundaryInputs, boundary_secret_values};
        let file = config();
        let values = HashMap::from([(
            "PROBE_ENDPOINT".into(),
            "postgres://fixture-user:fixture-value@localhost/fixture".into(),
        )]);
        let (secrets, complete) = boundary_secret_values(
            &BoundaryInputs {
                config: &file,
                environment: &values,
                sandbox_secrets: &[],
                sandbox_secrets_complete: true,
                overrides_base_url: "",
            },
            None,
        );
        assert!(complete);
        assert!(secrets.iter().any(|value| value == "fixture-value"));
        let snapshot = otto::sandbox::environment::resolve_environment(
            &otto::sandbox::environment::EnvironmentOptions {
                host_entries: vec![b"PROBE_ENDPOINT=opaque-service-descriptor".to_vec()],
                provider_names: vec!["PROBE_ENDPOINT".into()],
                allow_names: vec!["PROBE_ENDPOINT".into()],
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            snapshot
                .entries()
                .unwrap()
                .iter()
                .all(|entry| !entry.starts_with("PROBE_ENDPOINT="))
        );
        let endpoint = otto::runtime::storage::StorageEndpoint::Service(Arc::from(
            "opaque-service-descriptor",
        ));
        assert!(!format!("{endpoint:?}").contains("opaque-service-descriptor"));
        use otto_core::config::layered::{Layer, Merged};
        let mut merged = Merged::default();
        merged
            .add(&toml::to_string(&file).unwrap(), "user", Layer::User, false)
            .unwrap();
        let mut changed = file.clone();
        changed
            .storage
            .backends
            .get_mut("primary")
            .unwrap()
            .endpoint_env = Some("OTHER_ENDPOINT".into());
        merged
            .add(
                &toml::to_string(&changed).unwrap(),
                "project",
                Layer::Shared,
                true,
            )
            .unwrap();
        assert_eq!(merged.file().unwrap().storage, file.storage);
        assert!(
            !toml::to_string(&merged.explained().values)
                .unwrap()
                .contains("PROBE_ENDPOINT")
        );
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let backend = Arc::new(JsonBackend::new(
            temp.path().join("state.json"),
            "probe:startup",
        ));
        let registry = make_registry(backend);
        let home = temp.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(&path, toml::to_string(&file).unwrap()).unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let init = vec![
            "init".into(),
            home.display().to_string(),
            path.display().to_string(),
        ];
        assert_eq!(
            otto_cli::storage::run_async_with_backends(
                &init,
                &mut out,
                &mut err,
                &registry,
                &environment()
            )
            .await,
            0
        );
        let args = vec![
            "memory".into(),
            "status".into(),
            "--config".into(),
            path.display().to_string(),
        ];
        let entries = vec![
            format!("HOME={}", home.display()).into_bytes(),
            b"PROBE_ENDPOINT=opaque-service-descriptor".to_vec(),
        ];
        out.clear();
        err.clear();
        assert_eq!(
            otto_cli::run::run_with_backends(
                &args,
                Box::new(std::io::Cursor::new(Vec::<u8>::new())),
                &mut out,
                &mut err,
                entries,
                false,
                &Default::default(),
                &Default::default(),
                &registry
            )
            .await,
            0,
            "{}",
            String::from_utf8_lossy(&err)
        );
        assert!(!String::from_utf8_lossy(&out).contains("opaque-service-descriptor"));
    }

    include!("external_storage_backend/registration_and_migration.rs");
}

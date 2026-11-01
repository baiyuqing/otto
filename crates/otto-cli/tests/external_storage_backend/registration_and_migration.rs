// Registration, restart and SQLite/JSON migration tests; included into `mod tests`.

    #[tokio::test]
    async fn external_registration_bootstrap_restart_and_namespace_guard() {
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let path = temp.path().join("external.json");
        let backend = Arc::new(JsonBackend::new(path.clone(), "probe:one"));
        let registry = make_registry(backend);
        let configuration = config();
        assert!(
            Storage::open_home_with(&home, &registry, &configuration.storage, &environment())
                .await
                .is_err()
        );
        let config_path = temp.path().join("target.toml");
        std::fs::write(&config_path, toml::to_string(&configuration).unwrap()).unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = vec![
            "init".into(),
            home.display().to_string(),
            config_path.display().to_string(),
        ];
        assert_eq!(
            otto_cli::storage::run_async_with_backends(
                &args,
                &mut out,
                &mut err,
                &registry,
                &environment()
            )
            .await,
            0,
            "{}",
            String::from_utf8_lossy(&err)
        );
        let storage =
            Storage::open_home_with(&home, &registry, &configuration.storage, &environment())
                .await
                .unwrap();
        let service =
            approval::Service::from_store(storage.approvals(&home.join("unused.db")).unwrap())
                .unwrap();
        service
            .set_mode(&proposal().scope, otto_core::approval::ApprovalMode::Manual)
            .unwrap();
        let request = service.create(proposal()).unwrap();
        let events = service
            .events("/contract", Some("memory:contract"), 0)
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(!home.join("unused.db").exists());
        drop(service);
        drop(storage);
        let reopened = make_registry(Arc::new(JsonBackend::new(path.clone(), "probe:one")));
        let storage =
            Storage::open_home_with(&home, &reopened, &configuration.storage, &environment())
                .await
                .unwrap();
        assert_eq!(
            storage
                .approvals(&home.join("unused.db"))
                .unwrap()
                .load(&request.id)
                .unwrap(),
            request
        );
        drop(storage);
        let wrong = make_registry(Arc::new(JsonBackend::new(path, "probe:other")));
        assert!(
            Storage::open_home_with(&home, &wrong, &configuration.storage, &environment())
                .await
                .is_err()
        );
        assert!(
            Storage::open_home_with(
                &home,
                &Default::default(),
                &configuration.storage,
                &environment()
            )
            .await
            .is_err()
        );
        let bytes = std::fs::read_to_string(home.join(".otto/storage/authority.json")).unwrap();
        assert!(!bytes.contains("opaque-service-descriptor"));
        err.clear();
        assert_eq!(
            otto_cli::storage::run_async_with_backends(
                &args,
                &mut out,
                &mut err,
                &reopened,
                &environment()
            )
            .await,
            1
        );
    }

    #[tokio::test]
    async fn sqlite_to_independent_json_and_reverse_preserve_authority_and_receipts() {
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let service = approval::Service::open(&home.join(".otto/approvals.db")).unwrap();
        service
            .set_mode(&proposal().scope, otto_core::approval::ApprovalMode::Manual)
            .unwrap();
        let expected = service.create(proposal()).unwrap();
        let events = service.events("/contract", None, 0).unwrap();
        drop(service);
        let backend = Arc::new(JsonBackend::new(
            temp.path().join("external.json"),
            "probe:migrated",
        ));
        backend.lose_import_ack.store(true, Ordering::SeqCst);
        let registry = make_registry(backend.clone());
        let target = temp.path().join("target.toml");
        std::fs::write(&target, toml::to_string(&config()).unwrap()).unwrap();
        let staging = temp.path().join("stage");
        let args = vec![
            "transfer".into(),
            home.display().to_string(),
            staging.display().to_string(),
            "first".into(),
            "--to-config".into(),
            target.display().to_string(),
        ];
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(
            otto_cli::storage::run_async_with_backends(
                &args,
                &mut out,
                &mut err,
                &registry,
                &environment()
            )
            .await,
            0,
            "{}",
            String::from_utf8_lossy(&err)
        );
        assert_eq!(
            otto_cli::storage::run_async_with_backends(
                &args,
                &mut out,
                &mut err,
                &registry,
                &environment()
            )
            .await,
            0
        );
        backend.check_import_receipts().await.unwrap();
        let wrong = make_registry(Arc::new(JsonBackend::new(
            temp.path().join("external.json"),
            "probe:other",
        )));
        assert_eq!(
            otto_cli::storage::run_async_with_backends(
                &args,
                &mut out,
                &mut err,
                &wrong,
                &environment()
            )
            .await,
            1
        );
        let config_dir = home.join(".config/otto");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::copy(&target, config_dir.join("config.toml")).unwrap();
        let storage = Storage::open_home_with(&home, &registry, &config().storage, &environment())
            .await
            .unwrap();
        let repository = storage.approvals(&home.join("unused.db")).unwrap();
        assert_eq!(repository.load(&expected.id).unwrap(), expected);
        assert_eq!(repository.events("/contract", None, 0).unwrap(), events);
        drop(repository);
        drop(storage);
        let destination = temp.path().join("reverse");
        let args = vec![
            "transfer".into(),
            home.display().to_string(),
            destination.display().to_string(),
            "reverse".into(),
        ];
        err.clear();
        assert_eq!(
            otto_cli::storage::run_async_with_backends(
                &args,
                &mut out,
                &mut err,
                &registry,
                &environment()
            )
            .await,
            0,
            "{}",
            String::from_utf8_lossy(&err)
        );
        // Explicit preference changes cannot bypass route publication in either direction.
        assert!(
            Storage::open_home_with(&home, &registry, &config().storage, &environment())
                .await
                .is_err()
        );
        let storage = Storage::open_home(&home).await.unwrap();
        assert_eq!(
            storage
                .approvals(&home.join("unused.db"))
                .unwrap()
                .load(&expected.id)
                .unwrap(),
            expected
        );
    }

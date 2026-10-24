//! The registered-domain transfer and its assembly.

use super::*;

pub(super) async fn transfer_registered(
    options: Options,
    registry: &crate::runtime::storage::BackendRegistry,
    environment: &HashMap<String, String>,
    destination_config: Option<&otto_core::config::File>,
) -> Result<migration::Manifest> {
    let home = pin_directory(&options.home).map_err(unavailable)?;
    let otto = mkdir(&home, ".otto")?;
    let _control = mkdir(&otto, "storage")?;
    sync_directory(&otto)?;
    sync_directory(&home)?;
    let control_path = options.home.join(".otto/storage");
    let source_identity = identity(&options.home);
    let cutover = LocalCutover::open(
        &control_path,
        RouteGroup {
            identity: source_identity,
            generation: "initial".into(),
            writer_epoch: "initial".into(),
            domains: BTreeMap::new(),
        },
    )?;
    let journal = LocalJournal::open(&control_path)?;
    let mut previous = journal.load(&options.id).await?;
    if let Some(entry) = &mut previous
        && entry.manifest.published_generation.is_none()
        && let Some(generation) = cutover.receipt(&options.id).await?
    {
        if !entry.manifest.sealed
            || entry.manifest.rolled_back
            || generation == entry.manifest.expected_route_generation
        {
            return Err(invalid(
                "publication receipt differs from the sealed manifest",
            ));
        }
        entry.manifest.published_generation = Some(generation);
        entry.revision = journal.save(Some(entry.revision), &entry.manifest).await?;
    }
    if let Some(entry) = &previous {
        if entry.manifest.destination_identity != identity(&options.destination) {
            return Err(invalid(
                "resume destination differs from the durable migration manifest",
            ));
        }
        if entry.manifest.published_generation.is_some() {
            if let Some(config) = destination_config {
                let published = cutover
                    .published_routes(&options.id)
                    .await?
                    .ok_or_else(|| invalid("published routing receipt is missing"))?;
                let bindings = crate::runtime::storage::backend::resolve(
                    &config.storage,
                    environment,
                    &options.home,
                )
                .map_err(|_| invalid("destination backend configuration is invalid"))?;
                for (domain, requested) in
                    crate::runtime::storage::backend::configured_routes(&config.storage)
                {
                    let route = published
                        .get(&domain)
                        .ok_or_else(|| invalid("resume domain differs from published routes"))?;
                    if route.backend != requested.backend || route.endpoint != requested.endpoint {
                        return Err(invalid("resume backend differs from the published route"));
                    }
                    let instance = requested
                        .endpoint
                        .strip_prefix("instance:")
                        .ok_or_else(|| invalid("invalid destination instance"))?;
                    let binding = &bindings[instance];
                    let factory = registry
                        .configured_factory(&binding.factory, &binding.endpoint, &binding.options)
                        .await
                        .map_err(|_| invalid("destination backend is unavailable"))?;
                    let requirement =
                        crate::runtime::storage::RepositoryRequirements::for_domain(&domain)
                            .map_err(|_| invalid("unsupported destination domain"))?;
                    let actual = factory
                        .validate(&binding.endpoint, &requirement, &binding.options)
                        .await
                        .map_err(|_| {
                            invalid("published destination is unavailable or incompatible")
                        })?;
                    if route.namespace_identity.as_deref() != Some(actual.0.as_str()) {
                        return Err(invalid(
                            "resume namespace differs from the publication receipt",
                        ));
                    }
                }
            }
            return Ok(entry.manifest.clone());
        }
        if entry.manifest.rolled_back {
            return Err(invalid("migration was rolled back; use a new migration ID"));
        }
    }
    let routes = cutover.routes().await?;
    // Memory is not a storage domain. A group an earlier release published
    // with a `memory` route cannot be frozen or published without it.
    if routes.domains.contains_key("memory") {
        return Err(invalid(
            "published storage routes include memory, which this version does not transfer",
        ));
    }
    let supported = [
        "approvals",
        "workflows",
        "schedule",
        "usage",
        "tasks",
        "skill_checks",
        "memory_advice",
        "sessions",
    ];
    if routes
        .domains
        .keys()
        .any(|domain| !supported.contains(&domain.as_str()))
    {
        return Err(invalid(
            "source routing contains a state domain unsupported by offline transfer",
        ));
    }
    let source_config_path = options.home.join(".config/otto/config.toml");
    let source_config = if present_file(&source_config_path)? {
        read_configuration(&source_config_path)?
    } else {
        Default::default()
    };
    let source_bindings = crate::runtime::storage::backend::resolve(
        &source_config.storage,
        environment,
        &options.home,
    )
    .map_err(|_| invalid("source backend configuration is invalid"))?;
    let mut external_sources = Vec::new();
    for (domain, route) in &routes.domains {
        if route.backend == "posix"
            || route.backend == "sqlite" && !route.endpoint.starts_with("instance:")
        {
            continue;
        }
        let instance = route
            .endpoint
            .strip_prefix("instance:")
            .ok_or_else(|| invalid("external source requires a configured backend instance"))?;
        let binding = source_bindings
            .get(instance)
            .ok_or_else(|| invalid("source backend instance is not configured"))?
            .clone();
        if binding.factory != route.backend {
            return Err(invalid(
                "source backend instance differs from the published route",
            ));
        }
        let factory = registry
            .configured_factory(&binding.factory, &binding.endpoint, &binding.options)
            .await
            .map_err(|_| invalid("source backend is unavailable"))?;
        let requirements = crate::runtime::storage::RepositoryRequirements::for_domain(domain)
            .map_err(|_| invalid("unsupported source domain"))?;
        let actual = factory
            .validate(&binding.endpoint, &requirements, &binding.options)
            .await
            .map_err(|_| invalid("source backend profile is unavailable or incompatible"))?;
        if route.namespace_identity.as_deref() != Some(actual.0.as_str()) {
            return Err(invalid(
                "source backend namespace differs from the published route",
            ));
        }
        external_sources.push(ExternalDomain {
            domain: domain.clone(),
            factory,
            binding,
        });
    }
    let databases = databases(&options, &routes.domains)?;
    let session_root = match routes.domains.get("sessions") {
        Some(route) if route.backend == "posix" => absolute(Path::new(&route.endpoint))?,
        Some(_) => {
            return Err(invalid(
                "source sessions backend is unsupported by offline transfer",
            ));
        }
        None => options.home.join(".otto/sessions"),
    };
    if options.destination.starts_with(&session_root)
        || session_root.starts_with(&options.destination)
    {
        return Err(invalid(
            "source session namespace and destination root must not overlap",
        ));
    }
    let session_inputs = match pin_directory(&session_root) {
        Ok(_) => Some(journal_inputs(&session_root)?),
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && !routes.domains.contains_key("sessions") =>
        {
            None
        }
        Err(error) => return Err(unavailable(error)),
    };
    if databases.is_empty() && external_sources.is_empty() && session_inputs.is_none() {
        return Err(invalid("source contains no supported persistent state"));
    }
    let staged_root = destination(&options, &routes.identity)?;
    let staged_sessions = options.destination.join("sessions");
    if session_inputs.is_some() {
        mkdir(&staged_root, "sessions")?;
        sync_directory(&staged_root)?;
    }
    let mut source_routes = BTreeMap::new();
    let mut destination_routes = BTreeMap::new();
    let mut staged_databases = BTreeMap::new();
    for database in &databases {
        let domain = database.codec.domain().to_owned();
        let target = options.destination.join(format!("{domain}.db"));
        source_routes.insert(
            domain.clone(),
            StorageRoute {
                backend: "sqlite".into(),
                endpoint: endpoint(&database.path)?,
                namespace_identity: None,
            },
        );
        destination_routes.insert(
            domain.clone(),
            StorageRoute {
                backend: "sqlite".into(),
                endpoint: endpoint(&target)?,
                namespace_identity: None,
            },
        );
        staged_databases.insert(domain, target);
    }
    if session_inputs.is_some() {
        source_routes.insert(
            "sessions".into(),
            StorageRoute {
                backend: "posix".into(),
                endpoint: endpoint(&session_root)?,
                namespace_identity: None,
            },
        );
        destination_routes.insert(
            "sessions".into(),
            StorageRoute {
                backend: "posix".into(),
                endpoint: endpoint(&staged_sessions)?,
                namespace_identity: None,
            },
        );
    }
    for source in &external_sources {
        source_routes.insert(
            source.domain.clone(),
            routes.domains[&source.domain].clone(),
        );
        let target = options.destination.join(format!("{}.db", source.domain));
        destination_routes.insert(
            source.domain.clone(),
            StorageRoute {
                backend: "sqlite".into(),
                endpoint: endpoint(&target)?,
                namespace_identity: None,
            },
        );
        staged_databases.insert(source.domain.clone(), target);
    }
    let generation = format!("frozen:{}", options.id);
    let fence: Arc<dyn FreezeGuard> = Arc::new(Fence(cutover.clone()));
    let context = crate::runtime::storage::backend::MigrationContext {
        migration_id: options.id.clone(),
        source_identity: routes.identity.clone(),
        destination_identity: identity(&options.destination),
        generation: generation.clone(),
        fence: fence.clone(),
    };
    let mut imported_stages = BTreeMap::new();
    if let Some(config) = destination_config {
        let bindings =
            crate::runtime::storage::backend::resolve(&config.storage, environment, &options.home)
                .map_err(|_| invalid("destination backend configuration is invalid"))?;
        for (domain, instance) in &config.storage.routes {
            if !source_routes.contains_key(domain) {
                return Err(invalid(
                    "destination route names a domain absent from the source snapshot",
                ));
            }
            let binding = &bindings[instance];
            let factory = registry
                .configured_factory(&binding.factory, &binding.endpoint, &binding.options)
                .await
                .map_err(|_| invalid("destination backend is unavailable"))?;
            let requirement = crate::runtime::storage::RepositoryRequirements::for_domain(domain)
                .map_err(|_| invalid("unsupported destination domain"))?;
            let prepared = factory
                .prepare_import(&binding.endpoint, &requirement, &binding.options, &context)
                .await
                .map_err(|_| invalid("destination logical import preparation failed"))?;
            prepared
                .namespace_identity
                .validate()
                .map_err(|_| invalid("destination namespace identity is invalid"))?;
            imported_stages.insert(domain.clone(), prepared.stage);
            staged_databases.remove(domain);
            destination_routes.insert(
                domain.clone(),
                StorageRoute {
                    backend: binding.factory.clone(),
                    endpoint: format!("instance:{instance}"),
                    namespace_identity: Some(prepared.namespace_identity.0),
                },
            );
        }
    }
    let prepared = PreparedCutover {
        migration_id: options.id.clone(),
        source_identity: routes.identity.clone(),
        destination_identity: identity(&options.destination),
        source_generation: generation.clone(),
        expected_route_generation: routes.generation.clone(),
        source_routes,
        destination_routes,
    };
    let stopped = Arc::new(NativeStopped {
        databases: databases
            .iter()
            .map(|database| database.path.clone())
            .collect(),
        journals: session_inputs
            .as_ref()
            .map(|inputs| {
                inputs
                    .iter()
                    .map(|input| session_root.join(&input.relative_path))
                    .collect()
            })
            .unwrap_or_default(),
    });
    cutover.freeze(prepared.clone(), stopped).await?;
    let assembled = assemble(
        &databases,
        session_inputs,
        &session_root,
        &staged_sessions,
        &staged_databases,
        &prepared,
        fence,
        &external_sources,
        imported_stages,
    )
    .await;
    let (source, stage) = match assembled {
        Ok(value) => value,
        Err(error) => {
            if previous.is_none() {
                cutover.abort_unplanned(&options.id).await?;
            }
            return Err(error);
        }
    };
    let coordinator = Coordinator {
        source: &source,
        destination: &stage,
        journal: &journal,
        cutover: &cutover,
        max_chunk_bytes: CHUNK_BYTES,
    };
    let result = async {
        if previous.is_none() {
            coordinator
                .plan(
                    options.id.clone(),
                    prepared.destination_identity,
                    prepared.expected_route_generation,
                )
                .await?;
        }
        coordinator.run(&options.id).await
    }
    .await;
    let close = coordinator.close().await;
    if result.is_err() && journal.load(&options.id).await?.is_none() {
        cutover.abort_unplanned(&options.id).await?;
    }
    close?;
    result
}

#[allow(clippy::too_many_arguments)]
async fn assemble(
    databases: &[Database],
    session_inputs: Option<Vec<JournalInput>>,
    session_root: &Path,
    staged_sessions: &Path,
    staged_databases: &BTreeMap<String, PathBuf>,
    prepared: &PreparedCutover,
    fence: Arc<dyn FreezeGuard>,
    external_sources: &[ExternalDomain],
    mut imported_stages: BTreeMap<String, Arc<dyn StageImport>>,
) -> Result<(GroupSource, GroupStage)> {
    let portable = !external_sources.is_empty() || !imported_stages.is_empty();
    let mut sources: Vec<Arc<dyn SnapshotExport>> = Vec::new();
    let mut stages: BTreeMap<String, Arc<dyn StageImport>> = BTreeMap::new();
    for database in databases {
        let source = sql::Source::open(
            &database.path,
            prepared.source_identity.clone(),
            prepared.source_generation.clone(),
            fence.clone(),
            database.codec.clone(),
        );
        let source = match source {
            Ok(source) => source,
            Err(error) => {
                for source in &sources {
                    let _ = source.close().await;
                }
                return Err(error);
            }
        };
        sources.push(Arc::new(source));
        let stage = imported_stages
            .remove(database.codec.domain())
            .unwrap_or_else(|| {
                Arc::new(sql::Stage::new(
                    staged_databases[database.codec.domain()].clone(),
                    prepared.destination_identity.clone(),
                    database.codec.clone(),
                ))
            });
        stages.insert(database.codec.domain().into(), stage);
    }
    let context = crate::runtime::storage::backend::MigrationContext {
        migration_id: prepared.migration_id.clone(),
        source_identity: prepared.source_identity.clone(),
        destination_identity: prepared.destination_identity.clone(),
        generation: prepared.source_generation.clone(),
        fence: fence.clone(),
    };
    for source in external_sources {
        let requirements =
            crate::runtime::storage::RepositoryRequirements::for_domain(&source.domain)
                .map_err(|_| invalid("unsupported source domain"))?;
        let exported = match source
            .factory
            .export(
                &source.binding.endpoint,
                &requirements,
                &source.binding.options,
                &context,
            )
            .await
        {
            Ok(exported) => exported,
            Err(_) => {
                for source in &sources {
                    let _ = source.close().await;
                }
                return Err(invalid("source logical export is unavailable"));
            }
        };
        sources.push(exported);
        let stage = if let Some(stage) = imported_stages.remove(&source.domain) {
            stage
        } else {
            Arc::new(sql::Stage::new(
                staged_databases[&source.domain].clone(),
                prepared.destination_identity.clone(),
                codec(&source.domain)?,
            ))
        };
        stages.insert(source.domain.clone(), stage);
    }
    let journals = if let Some(inputs) = session_inputs {
        let source = JournalSource::open(
            session_root,
            Domain::Sessions,
            inputs,
            prepared.source_identity.clone(),
            prepared.source_generation.clone(),
            fence,
        );
        let source = match source {
            Ok(source) => source,
            Err(error) => {
                for source in &sources {
                    let _ = source.close().await;
                }
                return Err(error);
            }
        };
        sources.push(Arc::new(source));
        let stage = match JournalStage::new(
            staged_sessions,
            Domain::Sessions,
            prepared.destination_identity.clone(),
        ) {
            Ok(stage) => Arc::new(stage),
            Err(error) => {
                for source in &sources {
                    let _ = source.close().await;
                }
                return Err(error);
            }
        };
        stages.insert("sessions".into(), stage.clone());
        Some(stage)
    } else {
        None
    };
    let source = GroupSource::new(sources).await?;
    let closure: Arc<dyn crate::storage::migration::group::ReferenceClosure> = if portable {
        Arc::new(
            crate::storage::migration::references::PortableReferenceClosure {
                stages: stages.clone(),
                max_bytes: 64 * 1024 * 1024,
            },
        )
    } else {
        // Retain the existing streaming native verifier and its established limits.
        Arc::new(
            crate::storage::backends::migration_references::NativeReferenceClosure::new(
                staged_databases.clone(),
                journals,
            ),
        )
    };
    match GroupStage::new(stages, closure) {
        Ok(stage) => Ok((source, stage)),
        Err(error) => {
            let _ = source.close().await;
            Err(error)
        }
    }
}

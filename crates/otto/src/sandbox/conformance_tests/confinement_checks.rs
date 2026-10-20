//! Checks that every advertised read, write and network confinement holds.

use super::*;

pub(crate) async fn canonical_paths_with_spaces_unicode_and_quotes(case: &dyn Contract) {
    let (fixture, _) = setup(case).await;
    let (_driver, executor) = open(case, &fixture).await;
    let (status, result, stdout) = run(
        &executor,
        shell_request(
            case,
            &fixture,
            r#"printf special-data > "$SANDBOX_CONFORMANCE_SPECIAL_PATH"; /bin/pwd"#,
        ),
    )
    .await;
    result.expect("Execute");
    assert_eq!(status.code, 0, "special-path status");
    let special = fixture.environment_value("SANDBOX_CONFORMANCE_SPECIAL_PATH");
    assert_eq!(
        std::fs::read_to_string(&special).expect("special-path file"),
        "special-data"
    );
    assert_eq!(
        stdout.trim_end(),
        fixture.workspace.to_str().expect("workspace text"),
        "pwd is not the workspace"
    );
    executor.close().expect("close");
}

pub(crate) async fn validation_errors_are_bounded(case: &dyn Contract) {
    let (fixture, _) = setup(case).await;
    let (_driver, executor) = open(case, &fixture).await;
    let sensitive = fixture.workspace.join("missing secret path");
    let mut request = shell_request(case, &fixture, "exit 0");
    request.dir = sensitive.clone();
    request.argv.push("secret-argument-value".to_string());
    request
        .env
        .push("SECRET_VALUE=secret-environment-value".to_string());
    let (_, result, _) = run(&executor, request).await;
    assert_eq!(result, Err(Error::InvalidRequest));
    let text = Error::InvalidRequest.to_string();
    for secret in [
        sensitive.to_str().expect("path text"),
        "secret-argument-value",
        "secret-environment-value",
    ] {
        assert!(
            !text.contains(secret),
            "validation error exposed request data: {text}"
        );
    }
    executor.close().expect("close");
}

pub(crate) async fn concurrent_calls_complete(case: &dyn Contract) {
    if case.skip_concurrent_calls() {
        eprintln!("SKIP concurrent_calls_complete: disabled for this driver");
        return;
    }
    const CALLERS: usize = 16;
    let (fixture, _) = setup(case).await;
    let (_driver, executor) = open(case, &fixture).await;
    let mut handles = Vec::with_capacity(CALLERS);
    for _ in 0..CALLERS {
        handles.push(spawn_execute(
            executor.clone(),
            shell_request(case, &fixture, "exit 0"),
            std::io::sink(),
            CancellationToken::new(),
        ));
    }
    for handle in handles {
        let (status, result) = await_execution(handle, "concurrent Execute").await;
        result.expect("concurrent Execute");
        assert_eq!((status.code, status.signaled), (0, false));
    }
    executor.close().expect("close");
}

pub(crate) async fn advertised_read_confinement(case: &dyn Contract) {
    let (fixture, capabilities) = setup(case).await;
    if !capabilities.read_confinement {
        return;
    }
    let (_driver, executor) = open(case, &fixture).await;
    const SCRIPT: &str = r#"/bin/cat "$SANDBOX_CONFORMANCE_READ_PATH""#;

    let local = fixture.with_environment("SANDBOX_CONFORMANCE_READ_PATH", &fixture.allowed_read);
    let (status, result, _) = run(&executor, shell_request(case, &local, SCRIPT)).await;
    result.expect("allowed read Execute");
    assert_eq!(status.code, 0, "allowed read status");

    let local = fixture.with_environment("SANDBOX_CONFORMANCE_READ_PATH", &fixture.outside_file);
    let (status, result, _) = run(&executor, shell_request(case, &local, SCRIPT)).await;
    result.expect("outside read Execute");
    assert_ne!(status.code, 0, "outside read was permitted");

    let link = fixture.workspace.join("outside read symlink");
    std::os::unix::fs::symlink(&fixture.outside_file, &link).expect("symlink");
    let local = fixture.with_environment("SANDBOX_CONFORMANCE_READ_PATH", &link);
    let (status, result, _) = run(&executor, shell_request(case, &local, SCRIPT)).await;
    result.expect("symlink read Execute");
    assert_ne!(status.code, 0, "symlink read was permitted");
    executor.close().expect("close");
}

pub(crate) async fn advertised_write_confinement(case: &dyn Contract) {
    let (fixture, capabilities) = setup(case).await;
    if !capabilities.write_confinement {
        return;
    }
    let (_driver, executor) = open(case, &fixture).await;

    let outside_write = fixture.base.join("outside write target");
    let local = fixture.with_environment("SANDBOX_CONFORMANCE_WRITE_PATH", &outside_write);
    let (status, result, _) = run(
        &executor,
        shell_request(
            case,
            &local,
            r#"printf denied > "$SANDBOX_CONFORMANCE_WRITE_PATH""#,
        ),
    )
    .await;
    result.expect("outside write Execute");
    assert_ne!(status.code, 0, "outside write was permitted");
    assert!(!outside_write.exists(), "outside write target exists");

    let target = fixture.base.join("outside symlink target");
    write_private(&target, "unchanged");
    let link = fixture.workspace.join("outside write symlink");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let local = fixture.with_environment("SANDBOX_CONFORMANCE_WRITE_PATH", &link);
    let (status, result, _) = run(
        &executor,
        shell_request(
            case,
            &local,
            r#"printf changed > "$SANDBOX_CONFORMANCE_WRITE_PATH""#,
        ),
    )
    .await;
    result.expect("symlink write Execute");
    assert_ne!(status.code, 0, "symlink write was permitted");
    assert_eq!(
        std::fs::read_to_string(&target).expect("symlink target"),
        "unchanged",
        "outside symlink target changed"
    );
    executor.close().expect("close");
}

pub(crate) async fn advertised_local_tcp_allow(case: &dyn Contract) {
    let (fixture, capabilities) = setup(case).await;
    let Some(policy) = policy_for_network(capabilities, NetworkMode::Allow) else {
        return;
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("tcp listener");
    let address = listener.local_addr().expect("listener address");
    let accepted = tokio::task::spawn_blocking(move || listener.accept().map(|_| ()));
    let (_driver, executor) = open_with_policy(case, &fixture, policy).await;
    let argv = case.tcp_client(&address.ip().to_string(), &address.port().to_string());
    let (status, result, _) = run(&executor, case.request(&fixture, argv)).await;
    result.expect("TCP client Execute");
    assert_eq!(status.code, 0, "TCP client status");
    tokio::time::timeout(AWAIT, accepted)
        .await
        .expect("timed out waiting for the allowed TCP accept")
        .expect("accept task")
        .expect("TCP accept");
    executor.close().expect("close");
}

pub(crate) async fn advertised_local_tcp_deny(case: &dyn Contract) {
    let (fixture, capabilities) = setup(case).await;
    let Some(policy) = policy_for_network(capabilities, NetworkMode::Deny) else {
        return;
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("tcp listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let address = listener.local_addr().expect("listener address");
    let (_driver, executor) = open_with_policy(case, &fixture, policy).await;
    let argv = case.tcp_client(&address.ip().to_string(), &address.port().to_string());
    let (status, result, _) = run(&executor, case.request(&fixture, argv)).await;
    result.expect("TCP deny Execute");
    assert_ne!(status.code, 0, "TCP deny status");
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "TCP listener accepted a connection despite NetworkDeny"
    );
    executor.close().expect("close");
}

pub(crate) async fn advertised_unix_socket_deny(case: &dyn Contract) {
    let (fixture, capabilities) = setup(case).await;
    if !capabilities.unix_socket_deny {
        return;
    }
    let _ = std::fs::remove_file(&fixture.unix_socket);
    let listener = std::os::unix::net::UnixListener::bind(&fixture.unix_socket).expect("listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let (_driver, executor) = open(case, &fixture).await;
    let argv = case.unix_client(&fixture.unix_socket);
    let (status, result, _) = run(&executor, case.request(&fixture, argv)).await;
    result.expect("Unix deny Execute");
    assert_ne!(status.code, 0, "Unix deny status");
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "Unix listener accepted a connection despite UnixSocketDeny"
    );
    executor.close().expect("close");
}

/// A confined command cannot read another process's environment, which is
/// where package credential bindings are delivered. Only confining drivers
/// run this check; an unconfined child of the same user can read it.
pub(crate) async fn sibling_process_environment_is_unreadable(case: &dyn Contract) {
    let (fixture, _) = setup(case).await;
    let marker = "sibling-environment-marker";
    // Platform binaries never expose their environment; package commands are
    // not platform binaries, so the sibling is an ad hoc signed copy.
    let sleeper = fixture.base.join("sibling-sleep");
    std::fs::copy("/bin/sleep", &sleeper).expect("sibling binary");
    let signed = std::process::Command::new("/usr/bin/codesign")
        .args(["-s", "-", "-f"])
        .arg(&sleeper)
        .output()
        .expect("codesign");
    assert!(signed.status.success(), "ad hoc signing failed");
    let mut sibling = std::process::Command::new(&sleeper)
        .arg("30")
        .env("SANDBOX_CONFORMANCE_SIBLING", marker)
        .spawn()
        .expect("sibling process");
    let probe = format!("/bin/ps -E -ww -o command= -p {}", sibling.id());
    let host = std::process::Command::new("/bin/sh")
        .args(["-c", &probe])
        .output()
        .expect("host probe");
    let (_driver, executor) = open(case, &fixture).await;
    let (_, result, stdout) = run(&executor, shell_request(case, &fixture, &probe)).await;
    let _ = sibling.kill();
    let _ = sibling.wait();
    result.expect("sibling probe Execute");
    assert!(
        String::from_utf8_lossy(&host.stdout).contains(marker),
        "host probe cannot read the sibling environment; the check proves nothing here"
    );
    assert!(
        !stdout.contains(marker),
        "confined command read the sibling environment: {stdout}"
    );
    executor.close().expect("close");
}

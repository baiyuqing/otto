use super::*;
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn exchanges_messages_and_reaps_process() {
    let manager = Manager::default();
    let spec = Spec {
        path: "/bin/cat".into(),
        args: Vec::new(),
        directory: std::env::current_dir().unwrap(),
        environment: Vec::new(),
        stdin: None,
    };
    let value = manager
        .run_interactive(
            spec,
            &CancellationToken::new(),
            Duration::from_millis(50),
            |stdout, mut stdin| async move {
                stdin
                    .write_all(b"hello\n")
                    .await
                    .map_err(|_| Error::ChildWait)?;
                let mut line = String::new();
                BufReader::new(stdout)
                    .read_line(&mut line)
                    .await
                    .map_err(|_| Error::ChildWait)?;
                Ok(line)
            },
        )
        .await
        .unwrap();
    assert_eq!(value, "hello\n");
    assert!(manager.state.lock().unwrap().active.is_empty());
}

#[tokio::test]
async fn finishing_protocol_terminates_descendant_processes() {
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("survived");
    let spec = Spec {
        path: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            "(/bin/sleep 0.5; echo bad > \"$1\") & echo ready; wait".into(),
            "sh".into(),
            marker.to_string_lossy().into_owned(),
        ],
        directory: directory.path().to_path_buf(),
        environment: Vec::new(),
        stdin: None,
    };
    let manager = Manager::default();
    manager
        .run_interactive(
            spec,
            &CancellationToken::new(),
            Duration::from_millis(50),
            |stdout, _stdin| async move {
                let mut line = String::new();
                BufReader::new(stdout)
                    .read_line(&mut line)
                    .await
                    .map_err(|_| Error::ChildWait)?;
                assert_eq!(line, "ready\n");
                Ok(())
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(650)).await;
    assert!(!marker.exists());
}

#[tokio::test]
async fn agent_exit_cancels_a_blocked_protocol_and_reaps_group() {
    let manager = Manager::default();
    let cancel = CancellationToken::new();
    let protocol_cancel = cancel.clone();
    let spec = Spec {
        path: "/usr/bin/true".into(),
        args: Vec::new(),
        directory: std::env::current_dir().unwrap(),
        environment: Vec::new(),
        stdin: None,
    };
    let result: Result<(), Error> = manager
        .run_interactive(
            spec,
            &cancel,
            Duration::from_millis(50),
            |_stdout, _stdin| async move {
                protocol_cancel.cancelled().await;
                Err(Error::Cancelled)
            },
        )
        .await;
    assert_eq!(result, Err(Error::ChildWait));
    assert!(cancel.is_cancelled());
    assert!(manager.state.lock().unwrap().active.is_empty());
}

#[tokio::test]
async fn reads_buffered_response_after_agent_exits() {
    let manager = Manager::default();
    let spec = Spec {
        path: "/bin/echo".into(),
        args: vec!["ready".into()],
        directory: std::env::current_dir().unwrap(),
        environment: Vec::new(),
        stdin: None,
    };
    let value = manager
        .run_interactive(
            spec,
            &CancellationToken::new(),
            Duration::from_millis(50),
            |stdout, _stdin| async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                let mut line = String::new();
                BufReader::new(stdout)
                    .read_line(&mut line)
                    .await
                    .map_err(|_| Error::ChildWait)?;
                Ok(line)
            },
        )
        .await
        .unwrap();
    assert_eq!(value, "ready\n");
}

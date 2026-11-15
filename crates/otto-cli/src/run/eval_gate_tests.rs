use super::*;
use std::io::Cursor;

async fn run_eval(args: &[&str], home: &std::path::Path) -> (i32, String, String) {
    let args: Vec<String> = args.iter().map(|argument| argument.to_string()).collect();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = run(
        &args,
        Box::new(Cursor::new(Vec::new())),
        &mut stdout,
        &mut stderr,
        vec![format!("HOME={}", home.to_string_lossy()).into_bytes()],
        false,
        &tokio_util::sync::CancellationToken::new(),
        &crate::terminate::Terminate::new(),
    )
    .await;
    (
        code,
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
    )
}

#[cfg(not(feature = "eval"))]
#[tokio::test]
async fn eval_without_the_feature_prints_the_not_compiled_line() {
    let home = tempfile::tempdir().expect("home");
    let (code, stdout, stderr) =
        run_eval(&["eval", "memory-retraction", "--dry-run"], home.path()).await;
    assert_eq!(code, 2);
    assert_eq!(stdout, "");
    assert_eq!(
        stderr,
        "otto: the eval command is not compiled into this binary; build with: cargo build -p otto-cli --features eval\n"
    );
}

#[cfg(feature = "eval")]
#[tokio::test]
async fn eval_dry_run_with_the_feature_prints_the_estimate() {
    let home = tempfile::tempdir().expect("home");
    let (code, stdout, stderr) =
        run_eval(&["eval", "memory-retraction", "--dry-run"], home.path()).await;
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(stdout.contains("estimate"), "{stdout}");
}


use super::*;

fn parse(args: &[&str]) -> Result<Parsed, ParseFailure> {
    let owned: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    parse_flags(&owned, &mut Vec::new())
}

fn options(args: &[&str]) -> CliOptions {
    match parse(args).expect("parse") {
        Parsed::Options(options) => *options,
        Parsed::Help | Parsed::Version => panic!("unexpected help or version"),
    }
}

fn rejection(args: &[&str]) -> String {
    match parse(args).expect_err("rejected") {
        ParseFailure::Rejected(message) => message,
        ParseFailure::Unsafe => panic!("unexpected unsafe parse"),
    }
}

#[test]
fn acp_tcp_listener_requires_serve() {
    let got = options(&["serve", "--acp-listen", "127.0.0.1:9001"]);
    assert_eq!(got.acp_listen, "127.0.0.1:9001");
    for args in [
        vec!["--acp-listen", "127.0.0.1:9001"],
        vec!["acp", "--acp-listen", "127.0.0.1:9001"],
    ] {
        let args = args.into_iter().map(str::to_string).collect::<Vec<_>>();
        assert!(matches!(
            parse_flags(&args, &mut Vec::new()),
            Err(ParseFailure::Rejected(_))
        ));
    }
}

#[test]
fn yolo_applies_only_to_a_single_session_frontend() {
    assert!(!options(&[]).yolo);
    assert!(options(&["--yolo"]).yolo);
    assert!(rejection(&["--yolo", "--attach"]).contains("--attach"));
    assert!(rejection(&["serve", "--yolo"]).contains("/auto yolo"));
    assert!(rejection(&["acp", "--yolo"]).contains("/auto yolo"));
    // The flag is documented in the usage text.
    let mut usage = Vec::new();
    print_usage(&mut usage);
    assert!(String::from_utf8(usage).unwrap().contains("--yolo"));
}

#[test]
fn defaults_the_workspace_to_the_current_directory() {
    let got = options(&[]);
    assert_eq!(got.cwd, ".");
    assert!(!got.serve);
    assert!(!got.explicit_config);
    assert_eq!(got.shell_timeout, Duration::ZERO);
    assert_eq!(got.max_output_bytes, 0);
}

#[test]
fn accepts_both_dash_forms_and_both_value_forms() {
    for args in [
        ["--model", "gpt-5"].as_slice(),
        ["-model", "gpt-5"].as_slice(),
        ["--model=gpt-5"].as_slice(),
        ["-model=gpt-5"].as_slice(),
    ] {
        assert_eq!(options(args).model, "gpt-5", "{args:?}");
    }
}

#[test]
fn records_which_options_were_written_explicitly() {
    let bare = options(&[]);
    assert!(!bare.prompt_set && !bare.sandbox_set && !bare.shell_time_set);
    assert!(!bare.max_output_set && !bare.socket_set && !bare.listen_set);

    let explicit = options(&[
        "--config",
        "/tmp/config.toml",
        "--prompt",
        "do it",
        "--sandbox",
        "off",
        "--shell-timeout",
        "30s",
        "--max-output-bytes",
        "4096",
    ]);
    assert!(explicit.explicit_config);
    assert!(explicit.prompt_set);
    assert!(explicit.sandbox_set);
    assert!(explicit.shell_time_set);
    assert!(explicit.max_output_set);
    assert_eq!(explicit.shell_timeout, Duration::from_secs(30));
    assert_eq!(explicit.max_output_bytes, 4096);
}

#[test]
fn a_repeated_flag_keeps_the_last_value() {
    assert_eq!(options(&["--model", "a", "--model", "b"]).model, "b");
}

#[test]
fn the_serve_subcommand_is_stripped_before_flag_scanning() {
    let got = options(&["serve", "--socket", "/tmp/otto.sock"]);
    assert!(got.serve);
    assert!(got.socket_set);
    assert_eq!(got.socket, "/tmp/otto.sock");
}

#[test]
fn the_acp_subcommand_accepts_runtime_flags() {
    let got = options(&[
        "acp",
        "--config",
        "/tmp/config.toml",
        "--cwd",
        "/work",
        "--profile",
        "p",
        "--provider",
        "openai-compatible",
        "--base-url",
        "http://127.0.0.1:1",
        "--model",
        "m",
        "--thinking",
        "low",
        "--sandbox",
        "off",
        "--shell-timeout",
        "30s",
        "--max-output-bytes",
        "4096",
    ]);
    assert!(got.acp && !got.serve);
    assert_eq!(got.cwd, "/work");
    assert_eq!(got.profile, "p");
    assert_eq!(got.sandbox, "off");
    assert!(got.explicit_config);
    assert!(!options(&["--cwd", "/work"]).acp);
}

#[test]
fn acp_attach_accepts_a_socket_and_acp_alone_does_not() {
    let got = options(&["acp", "--attach", "--socket", "/tmp/otto.sock"]);
    assert!(got.acp && got.attach && got.socket_set);
    assert_eq!(got.socket, "/tmp/otto.sock");
    assert!(options(&["acp", "--attach"]).attach);
    assert!(!options(&["acp"]).attach);
}

#[test]
fn attach_without_acp_selects_the_tui_client() {
    let got = options(&["--attach", "--socket", "/tmp/otto.sock", "--resume", "abc"]);
    assert!(got.attach && !got.acp && got.socket_set);
    assert_eq!(got.resume_path, "abc");
    assert!(options(&["--attach", "--continue"]).continue_last);
    assert!(options(&["--attach", "--ui", "tui"]).attach);
}

#[test]
fn browser_launch_flag_is_no_longer_supported() {
    let mut stdout = Vec::new();
    assert!(parse_flags(&["serve".into(), "--open".into()], &mut stdout).is_err());
}

#[test]
fn version_prints_build_version_and_stops() {
    for flag in ["--version", "-V"] {
        let mut stdout = Vec::new();
        let outcome = parse_flags(&[flag.to_string()], &mut stdout).expect("parse");
        assert_eq!(outcome, Parsed::Version);
        assert_eq!(
            String::from_utf8(stdout).expect("utf-8"),
            format!("otto {}\n", crate::VERSION)
        );
    }
}

#[test]
fn help_prints_usage_and_stops() {
    for flag in ["--help", "-h"] {
        let mut stdout = Vec::new();
        let outcome = parse_flags(&[flag.to_string()], &mut stdout).expect("parse");
        assert_eq!(outcome, Parsed::Help);
        let text = String::from_utf8(stdout).expect("utf-8 usage");
        assert!(text.starts_with("Usage: otto [options]\n"), "{text}");
        assert!(text.contains("       otto acp [options]"), "{text}");
        assert!(
            text.contains("       otto acp --attach [--socket PATH]"),
            "{text}"
        );
        assert!(
            text.contains("       otto --attach [--socket PATH] [--resume ID | --continue]"),
            "{text}"
        );
        assert!(text.contains(
            "  --sandbox MODE         sandbox mode: auto, seatbelt, or off (off is unsafe)\n"
        ));
        assert!(text.contains(
            "  --acp-listen HOST:PORT authenticated loopback ACP TCP (requires OTTO_ACP_TOKEN)\n"
        ));
    }
}

#[test]
fn an_unknown_flag_is_an_unsafe_parse() {
    assert_eq!(
        parse(&["--max-turns", "1"]).expect_err("rejected"),
        ParseFailure::Unsafe
    );
    assert_eq!(
        parse(&["--shell-timeout", "banana"]).expect_err("rejected"),
        ParseFailure::Unsafe
    );
    assert_eq!(
        parse(&["--max-output-bytes", "banana"]).expect_err("rejected"),
        ParseFailure::Unsafe
    );
    assert_eq!(
        parse(&["--model"]).expect_err("rejected"),
        ParseFailure::Unsafe
    );
    assert_eq!(
        parse(&["---x"]).expect_err("rejected"),
        ParseFailure::Unsafe
    );
}

#[test]
fn the_retired_approve_flag_names_its_replacement() {
    assert_eq!(
        rejection(&["--approve", "x"]),
        "otto: --approve was renamed to --prompt\n"
    );
}

#[test]
fn scanning_stops_at_the_first_positional_argument() {
    assert_eq!(
        rejection(&["extra"]),
        "otto: unexpected positional arguments\n"
    );
    assert_eq!(
        rejection(&["--", "--model", "x"]),
        "otto: unexpected positional arguments\n"
    );
    assert_eq!(rejection(&["-"]), "otto: unexpected positional arguments\n");
}

#[test]
fn every_invalid_flag_reports_its_allowed_values() {
    let cases: &[(&[&str], &str)] = &[
        (
            &["--sandbox", "docker"],
            "otto: --sandbox must be one of auto, seatbelt, off\n",
        ),
        (
            &["--continue", "--resume", "s.jsonl"],
            "otto: --continue and --resume cannot be used together\n",
        ),
        (
            &["--no-session", "--continue"],
            "otto: --no-session cannot be used with --continue or --resume\n",
        ),
        (
            &["--archive", "s.jsonl", "--continue"],
            "otto: --archive cannot be used with --continue\n",
        ),
        (
            &["--archive", "s.jsonl", "--resume", "other.jsonl"],
            "otto: --archive cannot be used with --resume\n",
        ),
        (
            &["--archive", "s.jsonl", "--no-session"],
            "otto: --archive cannot be used with --no-session\n",
        ),
        (
            &["--archive", "s.jsonl", "--prompt", "x"],
            "otto: --archive cannot be used with --prompt\n",
        ),
        (
            &["serve", "--ui", "repl"],
            "otto: serve cannot be combined with --ui\n",
        ),
        (
            &["serve", "--prompt", "x"],
            "otto: serve cannot be combined with --prompt\n",
        ),
        (
            &["serve", "--resume", "anything"],
            "otto: serve cannot be combined with --resume\n",
        ),
        (
            &["serve", "--continue"],
            "otto: serve cannot be combined with --continue\n",
        ),
        (
            &["serve", "--archive", "anything"],
            "otto: serve cannot be combined with --archive\n",
        ),
        (
            &["serve", "--no-session"],
            "otto: serve cannot be combined with --no-session\n",
        ),
        (
            &["--socket", "/tmp/otto.sock"],
            "otto: --socket requires the serve subcommand\n",
        ),
        (
            &["--listen", "127.0.0.1:0"],
            "otto: --listen requires the serve subcommand\n",
        ),
        (
            &["acp", "--ui", "repl"],
            "otto: acp cannot be combined with --ui\n",
        ),
        (
            &["acp", "--prompt", "x"],
            "otto: acp cannot be combined with --prompt\n",
        ),
        (
            &["acp", "--resume", "anything"],
            "otto: acp cannot be combined with --resume\n",
        ),
        (
            &["acp", "--continue"],
            "otto: acp cannot be combined with --continue\n",
        ),
        (
            &["acp", "--archive", "anything"],
            "otto: acp cannot be combined with --archive\n",
        ),
        (
            &["acp", "--no-session"],
            "otto: acp cannot be combined with --no-session\n",
        ),
        (
            &["acp", "--socket", "/tmp/otto.sock"],
            "otto: --socket requires the serve subcommand\n",
        ),
        (
            &["serve", "--attach"],
            "otto: --attach cannot be combined with serve\n",
        ),
        (
            &["--attach", "--prompt", "x"],
            "otto: --attach cannot be combined with --prompt\n",
        ),
        (
            &["--attach", "--no-session"],
            "otto: --attach cannot be combined with --no-session\n",
        ),
        (
            &["--attach", "--archive", "/tmp/x.jsonl"],
            "otto: --attach cannot be combined with --archive\n",
        ),
        (
            &["--attach", "--ui", "repl"],
            "otto: --attach cannot be combined with --ui repl\n",
        ),
        (
            &["--attach", "--resume", "abc", "--continue"],
            "otto: --continue and --resume cannot be used together\n",
        ),
        (
            &["acp", "--attach", "--listen", "127.0.0.1:0"],
            "otto: --listen requires the serve subcommand\n",
        ),
        (
            &["acp", "--attach", "--prompt", "x"],
            "otto: acp cannot be combined with --prompt\n",
        ),
        (
            &["--shell-timeout", "0s"],
            "otto: --shell-timeout must be greater than zero\n",
        ),
        (
            &["--max-output-bytes", "0"],
            "otto: --max-output-bytes must be greater than zero\n",
        ),
        (
            &["--thinking", "banana"],
            "otto: --thinking must be one of low, medium, high, xhigh, max\n",
        ),
        (
            &["--prompt", "   "],
            "otto: --prompt requires a non-empty value\n",
        ),
        (
            &["--prompt", "x", "--ui", "tui"],
            "otto: --prompt cannot be used with --ui tui\n",
        ),
    ];
    for (args, want) in cases {
        assert_eq!(&rejection(args), want, "{args:?}");
    }
}

#[test]
fn valid_thinking_levels_and_sandbox_modes_are_accepted() {
    for level in ["low", "medium", "high", "xhigh", "max"] {
        assert_eq!(options(&["--thinking", level]).thinking, level);
    }
    for mode in ["auto", "seatbelt", "off"] {
        assert_eq!(options(&["--sandbox", mode]).sandbox, mode);
    }
}

#[test]
fn go_integer_and_boolean_syntax_is_accepted() {
    assert_eq!(parse_go_int("0x10"), Ok(16));
    assert_eq!(parse_go_int("1_000"), Ok(1000));
    assert_eq!(parse_go_int("-5"), Ok(-5));
    assert_eq!(parse_go_int(""), Err(()));
    assert_eq!(parse_go_bool("TRUE"), Ok(true));
    assert_eq!(parse_go_bool("F"), Ok(false));
    assert_eq!(parse_go_bool("yes"), Err(()));
    assert!(!options(&["--no-session=false"]).no_session);
}

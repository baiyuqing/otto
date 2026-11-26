use super::*;

fn segments(command: &str) -> Vec<Vec<String>> {
    analyze(command).segments
}

fn check(command: &str, complete: bool, expected: &[&[&str]]) {
    let analysis = analyze(command);
    let expected: Vec<Vec<String>> = expected
        .iter()
        .map(|words| words.iter().map(|word| word.to_string()).collect())
        .collect();
    assert_eq!(analysis.segments, expected, "{command:?}");
    assert_eq!(analysis.complete, complete, "{command:?}");
}

#[test]
fn quoting_and_escapes_follow_bash() {
    check(
        "git commit -m 'two words'",
        true,
        &[&["git", "commit", "-m", "two words"]],
    );
    check(
        r#"echo "a\"b" 'c\d' e\ f"#,
        true,
        &[&["echo", "a\"b", "c\\d", "e f"]],
    );
    check(
        r#"echo "\$HOME" "\\" "a\`b" "a\nb""#,
        true,
        &[&["echo", "$HOME", "\\", "a`b", "a\\nb"]],
    );
    check(
        "echo \"literal *\" '$HOME' a\"b c\"'d' '' a~",
        true,
        &[&["echo", "literal *", "$HOME", "ab cd", "", "a~"]],
    );
    check(r#""git" status"#, true, &[&["git", "status"]]);
    check("echo a\\\nb", true, &[&["echo", "ab"]]);
    check("rm -\\\nrf x", true, &[&["rm", "-rf", "x"]]);
    check(
        "gi\\\nt push \\\n origin",
        true,
        &[&["git", "push", "origin"]],
    );
    check("git pu\\\n$X", false, &[&["git"]]);
    assert!(!analyze("echo x >/dev/null\\\ny").complete);
    assert!(!analyze("A=1\\\nrm -rf x").complete);
}

#[test]
fn lists_pipelines_and_newlines_are_separate_segments() {
    check(
        "cargo fmt && cargo test || echo x; ls | wc -l",
        true,
        &[
            &["cargo", "fmt"],
            &["cargo", "test"],
            &["echo", "x"],
            &["ls"],
            &["wc", "-l"],
        ],
    );
    check("ls;", true, &[&["ls"]]);
    check("ls\npwd", true, &[&["ls"], &["pwd"]]);
}

#[test]
fn only_dev_null_and_fd_duplication_redirects_are_accepted() {
    for command in [
        "ls >/dev/null",
        "ls 2>/dev/null",
        "ls &>/dev/null",
        "ls >/dev/null 2>&1",
        "cargo test 2>&1 | tail -5",
    ] {
        assert!(analyze(command).complete, "{command}");
    }
    for command in [
        "ls > out.txt",
        "ls >> out.txt",
        "ls 2> err.txt",
        "ls < in.txt",
        "ls >/dev/null.x",
        "ls 1>&2",
    ] {
        let analysis = analyze(command);
        assert!(!analysis.complete, "{command}");
        assert_eq!(analysis.segments[0][0], "ls", "{command}");
    }
}

#[test]
fn dynamic_words_end_the_static_prefix_and_clear_complete() {
    check("echo $X", false, &[&["echo"]]);
    check("echo ${X}", false, &[&["echo"]]);
    check("echo \"a$X\"", false, &[&["echo"]]);
    check("echo a$X", false, &[&["echo"]]);
    check(
        r#"git push origin "$B""#,
        false,
        &[&["git", "push", "origin"]],
    );
    check("echo $((1+2))", false, &[&["echo"]]);
}

#[test]
fn nested_commands_are_collected_in_source_order() {
    check("echo $(git push)", false, &[&["echo"], &["git", "push"]]);
    check("echo `id`", false, &[&["echo"], &["id"]]);
    check("cat <(ls)", false, &[&["cat"], &["ls"]]);
    check("(git status)", false, &[&["git", "status"]]);
    check("{ git status; }", false, &[&["git", "status"]]);
    check(
        "if true; then git push; fi",
        false,
        &[&["true"], &["git", "push"]],
    );
    check("for i in a b; do echo $i; done", false, &[&["echo"]]);
    check("while true; do ls; done", false, &[&["true"], &["ls"]]);
    for command in ["case x in a) ls;; esac", "f() { ls; }"] {
        let analysis = analyze(command);
        assert!(!analysis.complete, "{command}");
        assert!(analysis.segments.contains(&vec!["ls".into()]), "{command}");
    }
}

#[test]
fn unsupported_syntax_clears_complete_but_keeps_static_words() {
    check("cat <<EOF\nhi\nEOF", false, &[&["cat"]]);
    check("cat <<< hi", false, &[&["cat"]]);
    check("X=1 git status", false, &[&["git", "status"]]);
    check("echo $'a'", false, &[&["echo"]]);
    check("echo $\"a\"", false, &[&["echo"]]);
    check("! git status", false, &[&["git", "status"]]);
    check("sleep 1 &", false, &[&["sleep", "1"]]);
    check("ls # note", false, &[&["ls"]]);
}

#[test]
fn malformed_input_is_incomplete() {
    assert!(!analyze("git commit -m 'x").complete);
    assert!(!analyze("echo (").complete);
    assert!(!analyze("ls &&").complete);
}

#[test]
fn unquoted_globs_and_tilde_are_not_static() {
    check("ls *.rs", false, &[&["ls"]]);
    check("ls a?", false, &[&["ls"]]);
    check("ls [a]", false, &[&["ls"]]);
    check("ls ~", false, &[&["ls"]]);
    check("ls ~/x", false, &[&["ls"]]);
    check("echo {a,b}", false, &[&["echo"]]);
    check("echo \"*\" '?' \\*", true, &[&["echo", "*", "?", "*"]]);
}

#[test]
fn runners_assignments_and_keywords_clear_complete() {
    check("bash -c x", false, &[&["bash", "-c", "x"]]);
    check("/bin/bash -lc x", false, &[&["/bin/bash", "-lc", "x"]]);
    check("env x", false, &[&["env", "x"]]);
    check("sudo ls", false, &[&["sudo", "ls"]]);
    check("'a=b' x", false, &[&["a=b", "x"]]);
    check("'if' x", false, &[&["if", "x"]]);
    check("'' x", false, &[&["", "x"]]);
    check("git status", true, &[&["git", "status"]]);
    check("/usr/bin/git status", true, &[&["/usr/bin/git", "status"]]);
}

#[test]
fn commands_over_64_kib_are_not_parsed() {
    let at_limit = format!("echo {}", "a".repeat(MAX_COMMAND_BYTES - 5));
    assert_eq!(at_limit.len(), MAX_COMMAND_BYTES);
    assert!(analyze(&at_limit).complete);
    let over = format!("echo {}", "a".repeat(MAX_COMMAND_BYTES - 4));
    assert_eq!(analyze(&over), ShellCommand::default());
    assert!(!analyze(&over).complete);
}

#[test]
fn output_redirect_targets_are_reported() {
    let targets = |command: &str| parse(command).write_targets;
    assert_eq!(
        targets("echo a >> out.txt"),
        vec![WriteTarget {
            path: "out.txt".into(),
            is_static: true
        }]
    );
    assert_eq!(
        targets("echo a > \"$F\""),
        vec![WriteTarget {
            path: "\"$F\"".into(),
            is_static: false
        }]
    );
    assert!(targets("ls 2>&1 < in.txt").is_empty());
    assert_eq!(
        segments("echo a > out.txt"),
        vec![vec!["echo".to_string(), "a".to_string()]]
    );
}

#[test]
fn git_subcommand_skips_global_options() {
    let words = |command: &str| -> Vec<String> { command.split(' ').map(str::to_owned).collect() };
    let sub = |command: &str| {
        let words = words(command);
        git_subcommand(&words).map(|(index, name)| (index, name.to_owned()))
    };
    assert_eq!(sub("git push origin"), Some((1, "push".into())));
    assert_eq!(sub("git -C dir push"), Some((3, "push".into())));
    assert_eq!(
        sub("git -c a=b --no-pager --git-dir=x --work-tree=y push"),
        Some((6, "push".into()))
    );
    assert_eq!(sub("/usr/bin/git status"), Some((1, "status".into())));
    assert_eq!(sub("git --bare push"), None);
    assert_eq!(sub("git -C"), None);
    assert_eq!(sub("git"), None);
    assert_eq!(sub("echo push"), None);
    // `commit -C <rev>` is a subcommand option, not a global one.
    assert_eq!(sub("git commit -C HEAD"), Some((1, "commit".into())));
}

fn classified(command: &str) -> Option<(&'static str, Vec<String>)> {
    let parsed = parse(command);
    classify(&parsed).map(|(effect, words)| (effect.as_str(), words.to_vec()))
}

#[test]
fn commands_keep_dynamic_words_as_source_text() {
    let parsed = parse(r#"find "$D" -name x -delete"#);
    assert_eq!(parsed.command.segments, vec![vec!["find".to_owned()]]);
    assert_eq!(
        parsed.commands,
        vec![
            ["find", "\"$D\"", "-name", "x", "-delete"]
                .map(str::to_owned)
                .to_vec()
        ]
    );
}

#[test]
fn classify_flags_external_state_and_destructive_commands() {
    for command in [
        "git push",
        "git push origin main",
        "git -C \"$D\" push",
        "timeout 30 git push",
        "env A=1 git push",
        "echo $(git push)",
        "make && git push origin main",
        "gh pr create",
        "gh",
        "gh api -f a=b x",
        "gh api -fa=b x",
        "gh api --field=a=b x",
        "gh api --input=f x",
        "gh api -X POST x",
        "gh api -XPOST x",
        "gh api --method POST x",
        "gh api --method=post x",
        "gh api -X",
        "curl \"$URL\" -d x",
        "curl -sSd x https://x",
        "curl -d@x https://x",
        "curl -T f https://x",
        "curl --json {} https://x",
        "curl --data-raw x https://x",
        "curl --form-string a=b https://x",
        "curl -X POST https://x",
        "curl -XDELETE https://x",
        "curl --request=PUT https://x",
        "curl -sX POST https://x",
        "curl -X",
        "wget --post-data=x https://x",
        "wget --method=PUT https://x",
        "wget --method DELETE https://x",
        "npm publish",
        "npm publish --tag x",
        "npm --registry x publish",
        "docker image push img",
        "git -P push",
        "git pu\\\nsh origin",
        "pnpm unpublish x",
        "yarn npm publish",
        "cargo +nightly publish",
        "cargo yank --vers 1",
        "docker push img",
        "podman -D push img",
        "twine upload dist/*",
        "gem push x.gem",
        "kubectl -n ns apply -f x",
        "helm upgrade x y",
        "terraform apply",
        "tofu destroy",
    ] {
        assert_eq!(
            classified(command).map(|c| c.0),
            Some("external_state"),
            "{command}"
        );
    }
    for command in [
        "git reset --hard",
        "git clean -f",
        "git clean --force -d",
        "git checkout -f x",
        "git checkout -- f",
        "git checkout .",
        "git restore f",
        "git restore --staged --worktree f",
        "git switch --discard-changes x",
        "git stash drop",
        "git stash clear",
        "git branch -D x",
        "git branch -d -f x",
        "git reflog expire --all",
        "git filter-branch x",
        "git filter-repo",
        "rm -rf x",
        "rm -r x",
        "rm -R x",
        "rm --recursive x",
        "sudo rm -rf x",
        "find . -delete",
        "find \"$D\" -delete",
        "find . -exec rm -r {} +",
        "xargs rm -r",
    ] {
        assert_eq!(
            classified(command).map(|c| c.0),
            Some("destructive"),
            "{command}"
        );
    }
}

#[test]
fn classify_ignores_read_only_and_unrelated_commands() {
    for command in [
        "ls",
        "git push --dry-run",
        "git push -n",
        "git status",
        "git restore --staged f",
        "git clean -n",
        "git clean -fn",
        "git branch -d x",
        "gh pr view 1",
        "gh api repos/o/r",
        "gh api -X GET x",
        "gh api -XGET x",
        "gh api --method=get x",
        "curl -I https://x",
        "curl -sS https://x",
        "curl -X GET https://x",
        "curl -XGET https://x",
        "curl -XHEAD https://x",
        "wget https://x",
        "wget --method=GET https://x",
        "rm file",
        "cargo build",
        "npm install",
        "git -P log",
        "find . -name x",
        "find . -exec ls {} +",
        "$GIT push",
        "bash -c 'git push'",
        "sudo ls",
    ] {
        assert_eq!(classified(command), None, "{command}");
    }
}

#[test]
fn classify_ranks_external_state_above_destructive() {
    let (effect, words) = classified("rm -r x && git push").unwrap();
    assert_eq!(effect, "external_state");
    assert_eq!(words, ["git", "push"]);
    let (effect, words) = classified("rm -r x && git reset --hard").unwrap();
    assert_eq!(effect, "destructive");
    assert_eq!(words, ["rm", "-r", "x"]);
    assert_eq!(
        classified("git push origin main").unwrap().1,
        ["git", "push", "origin", "main"]
    );
}

#[test]
fn complete_with_command_env_accepts_only_static_prefix_on_simple_command() {
    for command in [
        "cd sub && CARGO_TARGET_DIR=/x cargo test --test '*' && CARGO_TARGET_DIR=/x cargo test --doc",
        "cd sub && A=1 B='two' make",
        "A=1 cargo test",
        "A= cargo test",
    ] {
        let parsed = parse(command);
        assert!(!parsed.command.complete, "{command:?}");
        assert!(parsed.complete_with_command_env, "{command:?}");
    }
    for command in ["cargo test", "cd sub && make"] {
        let parsed = parse(command);
        assert!(parsed.command.complete, "{command:?}");
        assert!(parsed.complete_with_command_env, "{command:?}");
    }
    for command in [
        "CDPATH=/tmp; cd sub && make",
        "CDPATH=/tmp cd sub && make",
        "cd sub && X=$(pwd) make",
        "cd sub && X=\"$HOME\" make",
        "cd sub && A[$(pwd)]=1 make",
        "cd sub && export A=1 && make",
        "cd sub && make 2>&1 | $X",
    ] {
        let parsed = parse(command);
        assert!(!parsed.command.complete, "{command:?}");
        assert!(!parsed.complete_with_command_env, "{command:?}");
    }
}

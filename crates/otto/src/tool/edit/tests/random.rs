//! Randomized comparison of edits against a reference replacement.

use super::*;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }
}

fn text(rng: &mut Rng, alphabet: &[&str], max: usize) -> String {
    let len = rng.below(max + 1);
    (0..len).map(|_| rng.pick(alphabet)).collect()
}

fn count(haystack: &str, needle: &str) -> usize {
    occurrences(haystack, needle, |_| true).1
}

fn ascii(text: &str) -> String {
    normalize_for_fuzzy_match_with_map(text).0
}

fn edit(old: &str, new: &str) -> Vec<EditReplacement> {
    vec![EditReplacement {
        old_text: old.to_owned(),
        new_text: new.to_owned(),
    }]
}

fn sub(rng: &mut Rng, content: &str) -> Option<String> {
    let bounds: Vec<usize> = (0..=content.len())
        .filter(|i| content.is_char_boundary(*i))
        .collect();
    if bounds.len() < 2 {
        return None;
    }
    let a = rng.below(bounds.len() - 1);
    let b = a + 1 + rng.below(bounds.len() - 1 - a);
    Some(content[bounds[a]..bounds[b]].to_owned())
}

/// A unique exact match equals `replacen` on the content; any other count is an error.
#[test]
fn random_exact_edits_match_a_reference_replacement() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let file_chars = ["a", "b", "\n", " ", "\t", "\u{2019}", "x"];
    let new_chars = ["a", "c", "\n", " ", "\u{2014}"];
    let mut checked = 0;
    for _ in 0..300_000 {
        let content = text(&mut rng, &file_chars, 60);
        let nl = if rng.below(2) == 0 || !content.contains('\n') {
            "\n"
        } else {
            "\r\n"
        };
        let bom = rng.below(4) == 0;
        let body = content.replace('\n', nl);
        let file = if bom { format!("\u{feff}{body}") } else { body };
        let Some(old) = sub(&mut rng, &content) else {
            continue;
        };
        let new = text(&mut rng, &new_chars, 16);
        let result = apply_text_edits(&file, "f", &edit(&old, &new));
        match count(&content, &old) {
            1 => {
                let expected = content.replacen(&old, &new, 1).replace('\n', nl);
                let expected = if bom {
                    format!("\u{feff}{expected}")
                } else {
                    expected
                };
                let got = result.expect("unique exact match applies").0;
                assert_eq!(got, expected, "file={file:?} old={old:?} new={new:?}");
                checked += 1;
            }
            _ => assert!(result.is_err(), "file={file:?} old={old:?}"),
        }
    }
    assert!(
        checked >= 200_000,
        "only {checked} exact cases were checked"
    );
}

/// A fuzzy match changes only the matched lines, ignoring the whitespace the fuzzy view drops.
#[test]
fn random_fuzzy_edits_change_only_the_matched_lines() {
    let mut rng = Rng(0x0fed_cba9_8765_4321);
    let file_chars = [
        "a", "b", "\n", " ", " ", "\u{2019}", "\u{201c}", "\u{2014}", "\u{00a0}", "'", "-",
    ];
    let new_chars = ["a", "c", "\n", "'", "-", "\u{2019}"];
    let mut checked = 0;
    for _ in 0..300_000 {
        let content = text(&mut rng, &file_chars, 60);
        let nl = if rng.below(2) == 0 || !content.contains('\n') {
            "\n"
        } else {
            "\r\n"
        };
        let file = content.replace('\n', nl);
        let Some(raw_old) = sub(&mut rng, &content) else {
            continue;
        };
        let old = ascii(&raw_old);
        if old.trim().is_empty() || count(&content, &old) != 0 {
            continue;
        }
        let new = text(&mut rng, &new_chars, 16);
        let Ok((got, _, _)) = apply_text_edits(&file, "f", &edit(&old, &new)) else {
            continue;
        };
        // Line endings stay in the file's style.
        if nl == "\r\n" {
            assert!(
                !got.replace("\r\n", "").contains('\n'),
                "file={file:?} old={old:?} new={new:?} got={got:?}"
            );
        }
        let got_lf = got.replace("\r\n", "\n");
        // Up to the fuzzy folding, the result is the file with the match replaced by new_text.
        let fuzzy = ascii(&content);
        let expected = ascii(&fuzzy.replacen(&ascii(&old), &new, 1));
        // Whitespace the fuzzy view dropped from the file stays in place
        // while new_text may add or drop spaces, so compare without spaces.
        let visible = |text: &str| text.chars().filter(|c| *c != ' ').collect::<String>();
        assert_eq!(
            visible(&ascii(&got_lf)),
            visible(&expected),
            "file={file:?} old={old:?} new={new:?} got={got_lf:?}"
        );
        // Lines outside the matched line range keep their exact bytes. An
        // unchanged result has nothing to compare and is skipped first.
        if got_lf == content {
            checked += 1;
            continue;
        }
        let match_at = fuzzy.find(&ascii(&old)).unwrap();
        let match_first = fuzzy[..match_at].matches('\n').count();
        let match_last = fuzzy[..match_at + ascii(&old).len()].matches('\n').count();
        let before: Vec<&str> = content.split('\n').collect();
        let after: Vec<&str> = got_lf.split('\n').collect();
        let tail = before.len() - match_last - 1;
        assert!(
            before[..match_first] == after[..match_first]
                && after.len() >= tail
                && before[match_last + 1..] == after[after.len() - tail..],
            "change outside the matched lines: file={file:?} old={old:?} new={new:?} got={got_lf:?}"
        );
        checked += 1;
    }
    assert!(
        checked >= 150_000,
        "only {checked} fuzzy cases were checked"
    );
}

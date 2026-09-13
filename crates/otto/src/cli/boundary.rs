//! The secret-redaction boundary the whole process is gated on.
//!
//! Port of the `boundary*` and `collect*SecretValues` half of
//! `cmd/otto/runtime_builder.go`. Two questions are answered here:
//!
//! 1. Which exact strings must never reach the model or the terminal.
//! 2. Whether hiding them is provably complete.
//!
//! When the second answer is no, the binary refuses to send dynamic content at
//! all rather than send text it cannot prove is clean. That is why every
//! collection path returns `(values, false)` on the first overflow instead of
//! silently truncating: a redactor built from a partial set would look like it
//! was working.
//!
//! Completeness also fails when a secret value collides with fixed text. If
//! redacting the workspace path, a tool definition, or the system prompt
//! changes it, then the secret is a substring of text the model must see, and
//! no redaction can separate the two.
//!
//! Not yet ported: ChatGPT credentials from `otto login` (phase 5). Their
//! four values are collected by Go between the profile base URLs and the
//! session runtime; the seam is marked below.

use std::collections::HashMap;

use otto_core::agent::redactor::Redactor;
use otto_core::config::File;
use otto_core::config::resolve::Runtime;
use otto_core::model::ToolDefinition;
use otto_core::safetext::{MAX_SECRET_BYTES, SecretCollector, canonicalize_utf8};

use crate::gourl;
use crate::urlprivacy;

/// Everything fixed at startup that the boundary is computed from.
pub struct BoundaryInputs<'a> {
    /// Values the sandbox environment resolver already found, and whether it
    /// could prove it found all of them.
    pub sandbox_secrets: &'a [String],
    pub sandbox_secrets_complete: bool,
    pub config: &'a File,
    pub environment: &'a HashMap<String, String>,
    /// The `--base-url` override, which may itself carry userinfo.
    pub overrides_base_url: &'a str,
}

/// Accumulates secret forms and remembers the first overflow.
///
/// The collector is bounded on purpose (512 values, 1 MiB). Hitting the bound
/// is not an error to recover from: it means an unbounded set of secrets was
/// offered, so the boundary closes.
struct Collect {
    collector: SecretCollector,
    open: bool,
    complete: bool,
}

impl Collect {
    fn new(complete: bool) -> Self {
        Self {
            collector: SecretCollector::new(),
            open: true,
            complete,
        }
    }

    fn add(&mut self, value: &str) -> bool {
        if !self.open {
            return false;
        }
        if !self.collector.add(value) {
            self.open = false;
            self.complete = false;
            return false;
        }
        true
    }

    fn add_url(&mut self, raw: &str) {
        if !collect_url_secret_values(raw, self) {
            self.complete = false;
        }
    }

    fn values(&self) -> Vec<String> {
        self.collector.values()
    }
}

/// The secret values and whether the set is provably complete.
///
/// `runtime` is `None` before a profile has been resolved; the resolved
/// profile's own key and base URL are folded in once it exists.
pub fn boundary_secret_values(
    inputs: &BoundaryInputs<'_>,
    runtime: Option<&Runtime>,
) -> (Vec<String>, bool) {
    let mut collect = Collect::new(inputs.sandbox_secrets_complete);
    for value in inputs.sandbox_secrets {
        if !collect.add(value) {
            return (collect.values(), false);
        }
    }
    // The variable name is collected alongside its value: a model that learns
    // the name can ask a shell command to echo it.
    if !collect.add("OTTO_API_KEY") || !collect.add(environment_value(inputs, "OTTO_API_KEY")) {
        return (collect.values(), false);
    }
    for name in sorted_profile_names(inputs.config) {
        let profile = &inputs.config.profiles[&name];
        if !profile.api_key_env.is_empty()
            && (!collect.add(&profile.api_key_env)
                || !collect.add(environment_value(inputs, &profile.api_key_env)))
        {
            return (collect.values(), false);
        }
        collect.add_url(&profile.base_url);
        if !collect.open {
            return (collect.values(), false);
        }
    }
    collect.add_url(inputs.overrides_base_url);
    if !collect.open {
        return (collect.values(), false);
    }
    // Phase 5 seam: the four `auth.Credentials` values (access, refresh and
    // ID tokens, account id) are collected here in Go.
    if let Some(runtime) = runtime {
        if !collect.add(&runtime.api_key_env) || !collect.add(&runtime.api_key) {
            return (collect.values(), false);
        }
        collect.add_url(&runtime.base_url);
        if !collect.open {
            return (collect.values(), false);
        }
    }
    (collect.values(), collect.complete)
}

/// The redactor built from the collected values alone, before the
/// fixed-text collision check.
pub fn secret_redactor(inputs: &BoundaryInputs<'_>, runtime: Option<&Runtime>) -> Redactor {
    let (values, complete) = boundary_secret_values(inputs, runtime);
    Redactor::with_completeness(&values, complete)
}

/// A redactor that allows dynamic content only if redaction is complete *and*
/// leaves every piece of fixed text untouched.
///
/// `fixed` is the text that must survive redaction unchanged: the workspace
/// path, the resolved runtime's identifying fields, the tool definitions, and
/// the system prompt built from them.
pub fn boundary_redactor(
    inputs: &BoundaryInputs<'_>,
    runtime: Option<&Runtime>,
    fixed: &FixedText<'_>,
) -> Redactor {
    let boundary = secret_redactor(inputs, runtime);
    if !boundary.allows_dynamic_content() || !fields_unchanged(&boundary, runtime, fixed) {
        return Redactor::with_completeness(&[], false);
    }
    boundary
}

/// The fixed text a usable boundary must leave untouched.
#[derive(Default)]
pub struct FixedText<'a> {
    pub workspace_path: &'a str,
    /// `None` until the workspace exists; Go skips the definition and prompt
    /// checks in that case, because neither can be built yet.
    pub definitions: Option<&'a [ToolDefinition]>,
    pub system_prompt: Option<&'a str>,
}

/// Whether redaction leaves every piece of fixed text byte-identical.
pub fn fields_unchanged(
    redactor: &Redactor,
    runtime: Option<&Runtime>,
    fixed: &FixedText<'_>,
) -> bool {
    if !redactor.allows_dynamic_content() {
        return false;
    }
    if !fixed.workspace_path.is_empty() && !string_unchanged(redactor, fixed.workspace_path) {
        return false;
    }
    if let Some(runtime) = runtime {
        let endpoint_host = endpoint_host_for(&runtime.base_url);
        let context_window = runtime.compaction.context_window.to_string();
        let fields = [
            runtime.provider.as_str(),
            runtime.profile.as_str(),
            runtime.model.as_str(),
            runtime.thinking.as_str(),
            endpoint_host.as_str(),
            context_window.as_str(),
        ];
        if !fields.iter().all(|value| string_unchanged(redactor, value)) {
            return false;
        }
    }
    if let Some(definitions) = fixed.definitions
        && !definitions
            .iter()
            .all(|definition| definition_unchanged(redactor, definition))
    {
        return false;
    }
    match fixed.system_prompt {
        Some(prompt) => string_unchanged(redactor, prompt),
        None => true,
    }
}

/// The `host[:port]` of `base_url`, never its userinfo, path, or query, which
/// may carry credentials. Empty when the URL is absent or unparsable.
pub fn endpoint_host_for(base_url: &str) -> String {
    if base_url.is_empty() {
        return String::new();
    }
    match gourl::parse(base_url.as_bytes()) {
        Ok(parsed) => canonicalize_utf8(&parsed.host),
        Err(()) => String::new(),
    }
}

fn environment_value<'a>(inputs: &'a BoundaryInputs<'_>, name: &str) -> &'a str {
    inputs
        .environment
        .get(name)
        .map(String::as_str)
        .unwrap_or_default()
}

fn sorted_profile_names(config: &File) -> Vec<String> {
    let mut names: Vec<String> = config.profiles.keys().cloned().collect();
    names.sort();
    names
}

fn string_unchanged(redactor: &Redactor, value: &str) -> bool {
    redactor.redact_string(value) == value
}

/// Go walks the definition with reflection; the three fields it can reach are
/// the name, the description, and the parameter schema's keys and strings.
fn definition_unchanged(redactor: &Redactor, definition: &ToolDefinition) -> bool {
    if !string_unchanged(redactor, &definition.name)
        || !string_unchanged(redactor, &definition.description)
    {
        return false;
    }
    let Some(parameters) = definition.parameters.as_ref() else {
        return true;
    };
    match serde_json::from_str::<serde_json::Value>(parameters.get()) {
        Ok(value) => json_unchanged(redactor, &value),
        // Unparsable schema text cannot be proven clean, so close the
        // boundary rather than pass it through.
        Err(_) => false,
    }
}

fn json_unchanged(redactor: &Redactor, value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(text) => string_unchanged(redactor, text),
        serde_json::Value::Array(items) => items.iter().all(|item| json_unchanged(redactor, item)),
        serde_json::Value::Object(fields) => fields
            .iter()
            .all(|(key, field)| string_unchanged(redactor, key) && json_unchanged(redactor, field)),
        _ => true,
    }
}

/// Collects every form a credential embedded in `raw` could take.
///
/// Returns whether extraction is provably complete. A URL that does not parse
/// is never complete: it may still be sent somewhere that parses it more
/// leniently, credentials and all.
fn collect_url_secret_values(raw: &str, collect: &mut Collect) -> bool {
    if raw.is_empty() {
        return true;
    }
    if !collect.add(raw) {
        return false;
    }
    let complete = collect_raw_url_secret_values(raw, collect);

    let Ok(parsed) = gourl::parse(raw.as_bytes()) else {
        return false;
    };
    if let Some(user) = parsed.user.as_ref() {
        let username = canonicalize_utf8(user.username());
        if !collect.add(&username) {
            return false;
        }
        let mut decoded = username;
        if let Some(password) = user.password() {
            let password = canonicalize_utf8(password);
            if !collect.add(&password) {
                return false;
            }
            decoded.push(':');
            decoded.push_str(&password);
        }
        if !collect.add(&decoded) || !collect.add(&canonicalize_utf8(&user.encoded())) {
            return false;
        }
    }
    complete
}

/// Scans the unparsed text for credentials a parser would normalize away:
/// userinfo forms, query parameter values, and the fragment.
fn collect_raw_url_secret_values(raw: &str, collect: &mut Collect) -> bool {
    let (userinfo, ambiguous) = urlprivacy::userinfo_forms(raw.as_bytes());
    for value in userinfo {
        if !collect.add(&value) {
            return false;
        }
    }

    let query_start = raw.find('?');
    let fragment_start = raw.find('#');
    if let Some(start) = query_start
        && fragment_start.is_none_or(|fragment| start < fragment)
    {
        let end = fragment_start.unwrap_or(raw.len());
        for item in raw[start + 1..end].split(['&', ';']) {
            let Some((_, value)) = item.split_once('=') else {
                continue;
            };
            if !collect.add(value) {
                return false;
            }
            if let Ok(decoded) = gourl::unescape(value.as_bytes(), gourl::Encoding::QueryComponent)
                && !collect.add(&canonicalize_utf8(&decoded))
            {
                return false;
            }
        }
    }
    if let Some(start) = fragment_start {
        let fragment = &raw[start + 1..];
        if !collect.add(fragment) {
            return false;
        }
        if let Ok(decoded) = gourl::path_unescape(fragment.as_bytes())
            && !collect.add(&canonicalize_utf8(&decoded))
        {
            return false;
        }
        if let Ok(decoded) = gourl::unescape(fragment.as_bytes(), gourl::Encoding::QueryComponent)
            && !collect.add(&canonicalize_utf8(&decoded))
        {
            return false;
        }
    }
    if raw.len() > MAX_SECRET_BYTES {
        return false;
    }
    !ambiguous
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_core::agent::redactor::REDACTION_MARKER;
    use otto_core::config::Profile;

    fn config_with(profiles: &[(&str, &str, &str)]) -> File {
        let mut file = File::default();
        for (name, api_key_env, base_url) in profiles {
            file.profiles.insert(
                (*name).to_string(),
                Profile {
                    api_key_env: (*api_key_env).to_string(),
                    base_url: (*base_url).to_string(),
                    ..Profile::default()
                },
            );
        }
        file
    }

    fn environment(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn the_api_key_name_and_value_are_both_collected() {
        let config = config_with(&[("work", "WORK_KEY", "")]);
        let environment = environment(&[
            ("OTTO_API_KEY", "sk-default"),
            ("WORK_KEY", "sk-work"),
            ("UNRELATED", "visible"),
        ]);
        let inputs = BoundaryInputs {
            sandbox_secrets: &[],
            sandbox_secrets_complete: true,
            config: &config,
            environment: &environment,
            overrides_base_url: "",
        };
        let (values, complete) = boundary_secret_values(&inputs, None);
        assert!(complete);
        for expected in ["OTTO_API_KEY", "sk-default", "WORK_KEY", "sk-work"] {
            assert!(values.iter().any(|value| value == expected), "{values:?}");
        }
        assert!(!values.iter().any(|value| value == "visible"), "{values:?}");
    }

    #[test]
    fn the_resolved_runtime_key_is_folded_in() {
        let config = File::default();
        let environment = environment(&[]);
        let inputs = BoundaryInputs {
            sandbox_secrets: &[],
            sandbox_secrets_complete: true,
            config: &config,
            environment: &environment,
            overrides_base_url: "",
        };
        let runtime = Runtime {
            api_key: "sk-resolved".to_string(),
            api_key_env: "RESOLVED_KEY".to_string(),
            ..Runtime::default()
        };
        let (values, complete) = boundary_secret_values(&inputs, Some(&runtime));
        assert!(complete);
        assert!(values.iter().any(|value| value == "sk-resolved"));
        assert!(values.iter().any(|value| value == "RESOLVED_KEY"));
    }

    #[test]
    fn base_url_userinfo_is_collected_in_every_form() {
        let config = config_with(&[("work", "", "https://user:p%40ss@gw.example.com/v1")]);
        let environment = environment(&[]);
        let inputs = BoundaryInputs {
            sandbox_secrets: &[],
            sandbox_secrets_complete: true,
            config: &config,
            environment: &environment,
            overrides_base_url: "",
        };
        let (values, complete) = boundary_secret_values(&inputs, None);
        assert!(complete, "{values:?}");
        for expected in [
            "https://user:p%40ss@gw.example.com/v1",
            "user:p%40ss",
            "p@ss",
            "user:p@ss",
        ] {
            assert!(values.iter().any(|value| value == expected), "{values:?}");
        }
    }

    #[test]
    fn query_and_fragment_values_are_collected() {
        let config = File::default();
        let environment = environment(&[]);
        let inputs = BoundaryInputs {
            sandbox_secrets: &[],
            sandbox_secrets_complete: true,
            config: &config,
            environment: &environment,
            overrides_base_url: "https://gw.example.com/v1?token=sk-q%20uery#sk-frag",
        };
        let (values, _) = boundary_secret_values(&inputs, None);
        for expected in ["sk-q%20uery", "sk-q uery", "sk-frag"] {
            assert!(values.iter().any(|value| value == expected), "{values:?}");
        }
    }

    #[test]
    fn an_unparsable_base_url_closes_the_boundary() {
        let config = File::default();
        let environment = environment(&[]);
        let inputs = BoundaryInputs {
            sandbox_secrets: &[],
            sandbox_secrets_complete: true,
            config: &config,
            environment: &environment,
            overrides_base_url: "https://user:pass@exa mple.com/v1",
        };
        let (_, complete) = boundary_secret_values(&inputs, None);
        assert!(!complete);
        assert!(!secret_redactor(&inputs, None).allows_dynamic_content());
    }

    #[test]
    fn incomplete_sandbox_secrets_close_the_boundary() {
        let config = File::default();
        let environment = environment(&[]);
        let inputs = BoundaryInputs {
            sandbox_secrets: &["sk-live".to_string()],
            sandbox_secrets_complete: false,
            config: &config,
            environment: &environment,
            overrides_base_url: "",
        };
        let (values, complete) = boundary_secret_values(&inputs, None);
        assert!(!complete);
        assert!(values.iter().any(|value| value == "sk-live"));
        assert!(!secret_redactor(&inputs, None).allows_dynamic_content());
    }

    #[test]
    fn a_secret_that_collides_with_the_workspace_path_closes_the_boundary() {
        let redactor = Redactor::with_completeness(&["/home/dev/project".to_string()], true);
        assert!(redactor.allows_dynamic_content());
        let fixed = FixedText {
            workspace_path: "/home/dev/project",
            ..FixedText::default()
        };
        assert!(!fields_unchanged(&redactor, None, &fixed));
    }

    #[test]
    fn a_secret_that_collides_with_a_tool_definition_closes_the_boundary() {
        let redactor = Redactor::with_completeness(&["path".to_string()], true);
        let definitions = vec![ToolDefinition {
            name: "read".to_string(),
            description: "Reads a file".to_string(),
            parameters: Some(
                serde_json::value::RawValue::from_string(
                    r#"{"properties":{"path":{"type":"string"}}}"#.to_string(),
                )
                .expect("schema"),
            ),
        }];
        let fixed = FixedText {
            definitions: Some(&definitions),
            ..FixedText::default()
        };
        assert!(!fields_unchanged(&redactor, None, &fixed));

        let harmless = Redactor::with_completeness(&["sk-live".to_string()], true);
        assert!(fields_unchanged(&harmless, None, &fixed));
    }

    #[test]
    fn a_secret_that_collides_with_the_system_prompt_closes_the_boundary() {
        let redactor = Redactor::with_completeness(&["concise".to_string()], true);
        let prompt = "You are Otto, a concise coding agent.";
        let fixed = FixedText {
            system_prompt: Some(prompt),
            ..FixedText::default()
        };
        assert!(!fields_unchanged(&redactor, None, &fixed));
        assert!(
            !boundary_redactor(
                &BoundaryInputs {
                    sandbox_secrets: &["concise".to_string()],
                    sandbox_secrets_complete: true,
                    config: &File::default(),
                    environment: &environment(&[]),
                    overrides_base_url: "",
                },
                None,
                &fixed,
            )
            .allows_dynamic_content()
        );
    }

    #[test]
    fn a_secret_that_collides_with_a_runtime_field_closes_the_boundary() {
        let redactor = Redactor::with_completeness(&["gpt-test".to_string()], true);
        let runtime = Runtime {
            model: "gpt-test".to_string(),
            ..Runtime::default()
        };
        assert!(!fields_unchanged(
            &redactor,
            Some(&runtime),
            &FixedText::default()
        ));
    }

    #[test]
    fn an_endpoint_host_never_carries_userinfo_or_a_path() {
        assert_eq!(
            endpoint_host_for("https://user:pass@gw.example.com:8443/v1?token=x"),
            "gw.example.com:8443"
        );
        assert_eq!(endpoint_host_for(""), "");
        assert_eq!(endpoint_host_for("https://exa mple.com"), "");
    }

    #[test]
    fn a_usable_boundary_redacts_the_key_and_leaves_the_prompt_alone() {
        let config = config_with(&[("work", "WORK_KEY", "https://gw.example.com/v1")]);
        let environment = environment(&[("WORK_KEY", "sk-secret-value")]);
        let inputs = BoundaryInputs {
            sandbox_secrets: &[],
            sandbox_secrets_complete: true,
            config: &config,
            environment: &environment,
            overrides_base_url: "",
        };
        let prompt = "You are Otto, a concise coding agent.";
        let fixed = FixedText {
            workspace_path: "/tmp/workspace",
            definitions: None,
            system_prompt: Some(prompt),
        };
        let redactor = boundary_redactor(&inputs, None, &fixed);
        assert!(redactor.allows_dynamic_content());
        assert_eq!(redactor.redact_string(prompt), prompt);
        assert_eq!(
            redactor.redact_string("key is sk-secret-value here"),
            format!("key is {REDACTION_MARKER} here")
        );
    }
}

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use super::*;

/// The roles with an environment contract, by subcommand name.
const ROLES: &[(&str, &[EnvVar])] = &[
    ("api", API),
    ("migrate", MIGRATE),
    ("orchestrator", ORCHESTRATOR),
];

fn lookup_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k: &str| map.get(k).cloned()
}

fn all_required() -> Vec<(&'static str, &'static str)> {
    vec![
        ("DATABASE_URL", "postgres://ad:ad@db:5432/ad"),
        ("REDIS_URL", "redis://redis:6379"),
        ("LIVEKIT_URL", "ws://livekit:7880"),
        ("LIVEKIT_API_KEY", "key"),
        ("LIVEKIT_API_SECRET", "secret"),
        ("AD_SIP_TRUNK_ID", "ST_x"),
    ]
}

// ---- the parser ----

#[test]
fn orchestrator_reports_every_missing_required_var_at_once() {
    let err = OrchestratorConfig::from_lookup(lookup_from(&[])).unwrap_err();
    assert_eq!(
        err.missing,
        vec![
            "DATABASE_URL",
            "REDIS_URL",
            "LIVEKIT_URL",
            "LIVEKIT_API_KEY",
            "LIVEKIT_API_SECRET",
            "AD_SIP_TRUNK_ID",
        ]
    );
    let msg = err.to_string();
    assert!(msg.contains("REDIS_URL"), "{msg}");
    assert!(msg.contains("README.md"), "{msg}");
}

#[test]
fn empty_or_blank_value_counts_as_missing() {
    let mut pairs = all_required();
    pairs.retain(|(k, _)| *k != "REDIS_URL" && *k != "AD_SIP_TRUNK_ID");
    pairs.push(("REDIS_URL", ""));
    pairs.push(("AD_SIP_TRUNK_ID", "   "));
    let err = OrchestratorConfig::from_lookup(lookup_from(&pairs)).unwrap_err();
    assert_eq!(err.missing, vec!["REDIS_URL", "AD_SIP_TRUNK_ID"]);
}

#[test]
fn defaults_apply_when_optional_vars_are_unset() {
    let cfg = OrchestratorConfig::from_lookup(lookup_from(&all_required())).unwrap();
    assert_eq!(cfg.redis_url, "redis://redis:6379");
    assert_eq!(cfg.gateway.room_prefix, "campaign");
    assert_eq!(cfg.batch_limit, 20);
    assert_eq!(cfg.calls_per_sec, 10.0);
    assert_eq!(cfg.timer_poll_ms, 1000);
    assert_eq!(cfg.worker, "orchestrator");
    assert_eq!(cfg.per_call_gap(), Duration::from_millis(100));
}

#[test]
fn invalid_numbers_are_rejected_not_ignored() {
    let mut pairs = all_required();
    pairs.push(("AD_CALLS_PER_SEC", "fast"));
    pairs.push(("AD_BATCH_LIMIT", "0"));
    pairs.push(("AD_TIMER_POLL_MS", "-5"));
    let err = OrchestratorConfig::from_lookup(lookup_from(&pairs)).unwrap_err();
    assert!(err.missing.is_empty());
    assert_eq!(err.invalid.len(), 3, "{:?}", err.invalid);
    assert!(err.invalid[0].starts_with("AD_BATCH_LIMIT="));
    assert!(err.invalid[1].starts_with("AD_CALLS_PER_SEC="));
    assert!(err.invalid[2].starts_with("AD_TIMER_POLL_MS="));
}

#[test]
fn migrate_requires_database_url_and_api_requires_nothing() {
    let err = check_required("migrate", MIGRATE, lookup_from(&[])).unwrap_err();
    assert_eq!(err.missing, vec!["DATABASE_URL"]);
    assert!(check_required("api", API, lookup_from(&[])).is_ok());
}

// ---- drift: Helm chart, e2e scripts, README ----

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn contract(role: &str) -> &'static [EnvVar] {
    ROLES
        .iter()
        .find(|(r, _)| *r == role)
        .map(|(_, vars)| *vars)
        .unwrap_or_else(|| panic!("no contract for role {role}"))
}

fn known_names(role: &str) -> BTreeSet<&'static str> {
    contract(role).iter().map(|v| v.name).collect()
}

fn required_names(role: &str) -> BTreeSet<&'static str> {
    contract(role)
        .iter()
        .filter(|v| v.need == Need::Required)
        .map(|v| v.name)
        .collect()
}

fn is_env_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// The `{{ define "name" }}` bodies of a Helm helpers file. Nested
/// `if`/`range`/`with` blocks are tracked, so an inner `end` does not close the
/// define.
fn helm_defines(helpers: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut current: Option<(String, usize)> = None;
    let mut depth = 0usize;
    let mut rest = helpers;
    let mut pos = 0usize;
    while let Some(open) = rest.find("{{") {
        let Some(close) = rest[open..].find("}}") else {
            break;
        };
        let action = rest[open + 2..open + close]
            .trim_matches(|c: char| c == '-' || c.is_whitespace())
            .to_string();
        let action_end = pos + open + close + 2;
        if let Some(name) = action.strip_prefix("define ") {
            current = Some((name.trim().trim_matches('"').to_string(), action_end));
            depth = 1;
        } else if current.is_some()
            && (action.starts_with("if ")
                || action.starts_with("range ")
                || action.starts_with("with "))
        {
            depth += 1;
        } else if current.is_some() && action == "end" {
            depth -= 1;
            if depth == 0 {
                let (name, start) = current.take().unwrap();
                out.insert(name, helpers[start..pos + open].to_string());
            }
        }
        pos = action_end;
        rest = &helpers[pos..];
    }
    out
}

/// The env var names a Helm template sets, with `include`d helpers expanded.
fn helm_env(template: &str) -> BTreeSet<String> {
    let defines = helm_defines(&read("deploy/helm/agentdialer/templates/_helpers.tpl"));
    let mut text = read(&format!("deploy/helm/agentdialer/templates/{template}"));
    for (name, body) in &defines {
        if text.contains(&format!("include \"{name}\"")) {
            text.push('\n');
            text.push_str(body);
        }
    }
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("- name:"))
        .map(|n| n.trim().to_string())
        .filter(|n| is_env_name(n))
        .collect()
}

fn assert_env_matches_contract(source: &str, role: &str, set: &BTreeSet<String>) {
    let missing: Vec<_> = required_names(role)
        .into_iter()
        .filter(|r| !set.contains(*r))
        .collect();
    let known = known_names(role);
    let unknown: Vec<_> = set
        .iter()
        .filter(|n| !known.contains(n.as_str()))
        .cloned()
        .collect();
    assert!(
        missing.is_empty() && unknown.is_empty(),
        "{source} (role {role}): missing required {missing:?}; sets vars the binary does not read {unknown:?}"
    );
}

#[test]
fn helm_templates_wire_exactly_the_role_contract() {
    for (template, role) in [
        ("orchestrator.yaml", "orchestrator"),
        ("api.yaml", "api"),
        ("migrate-job.yaml", "migrate"),
    ] {
        let text = read(&format!("deploy/helm/agentdialer/templates/{template}"));
        assert!(
            text.contains(&format!("args: [\"{role}\"]")),
            "{template} must run the {role} role"
        );
        assert_env_matches_contract(template, role, &helm_env(template));
    }
}

/// Every `docker run ... agentdialer:e2e <role>` in the e2e scripts, as
/// (script, role, env names). Backslash continuations are joined. A variable
/// passed through a shell variable (for example `$LK_ENV`) is not seen, so the
/// scripts must pass each `-e NAME=` explicitly.
fn e2e_role_runs() -> Vec<(String, String, BTreeSet<String>)> {
    let mut runs = Vec::new();
    let dir = repo_root().join("e2e");
    let mut scripts: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "sh"))
        .collect();
    scripts.sort();
    for path in scripts {
        let text = std::fs::read_to_string(&path).unwrap();
        let joined = text.replace("\\\n", " ");
        for line in joined.lines() {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            if !tokens.windows(2).any(|w| w[0] == "docker" && w[1] == "run") {
                continue;
            }
            let Some(i) = tokens.iter().position(|t| *t == "agentdialer:e2e") else {
                continue;
            };
            let Some(role) = tokens.get(i + 1) else {
                continue;
            };
            let env: BTreeSet<String> = tokens
                .windows(2)
                .filter(|w| w[0] == "-e")
                .map(|w| w[1].split('=').next().unwrap_or("").to_string())
                .collect();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            runs.push((name, role.to_string(), env));
        }
    }
    runs
}

#[test]
fn e2e_scripts_pass_exactly_the_role_contract() {
    let runs = e2e_role_runs();
    assert!(
        runs.iter().any(|(_, role, _)| role == "orchestrator"),
        "expected at least one e2e script to start the orchestrator"
    );
    for (script, role, env) in &runs {
        assert_env_matches_contract(script, role, env);
    }
}

#[test]
fn readme_documents_every_role_variable() {
    let readme = read("README.md");
    let start = readme
        .find("## Configuration")
        .expect("README.md must have a \"## Configuration\" section");
    let section = &readme[start..];
    let section = &section[..section[3..].find("\n## ").map_or(section.len(), |i| i + 3)];
    for (role, vars) in ROLES {
        let heading = format!("### `{role}`");
        let sub_start = section
            .find(&heading)
            .unwrap_or_else(|| panic!("README Configuration needs a {heading} subsection"));
        let sub = &section[sub_start + heading.len()..];
        let sub = &sub[..sub.find("\n##").unwrap_or(sub.len())];
        for v in *vars {
            let row = sub
                .lines()
                .find(|l| l.starts_with(&format!("| `{}` |", v.name)))
                .unwrap_or_else(|| panic!("README {heading} has no row for `{}`", v.name));
            let says_required = row.contains("| required |");
            assert_eq!(
                says_required,
                v.need == Need::Required,
                "README {heading}: `{}` must be marked {}",
                v.name,
                if v.need == Need::Required {
                    "required"
                } else {
                    "optional"
                }
            );
        }
    }
}

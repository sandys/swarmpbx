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
        ("LIVEKIT_URL", "ws://livekit:7880"),
        ("LIVEKIT_API_KEY", "key"),
        ("LIVEKIT_API_SECRET", "secret"),
        ("AD_SIP_TRUNK", "carrier.example:5060"),
        (
            "AD_ETCD_ENDPOINTS",
            "http://kine-a:2379, http://kine-b:2379",
        ),
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
            "LIVEKIT_URL",
            "LIVEKIT_API_KEY",
            "LIVEKIT_API_SECRET",
            "AD_SIP_TRUNK",
            "AD_ETCD_ENDPOINTS",
        ]
    );
    let msg = err.to_string();
    assert!(msg.contains("AD_SIP_TRUNK"), "{msg}");
    assert!(msg.contains("README.md"), "{msg}");
}

#[test]
fn empty_or_blank_value_counts_as_missing() {
    let mut pairs = all_required();
    pairs.retain(|(k, _)| *k != "LIVEKIT_URL" && *k != "AD_SIP_TRUNK");
    pairs.push(("LIVEKIT_URL", ""));
    pairs.push(("AD_SIP_TRUNK", "   "));
    let err = OrchestratorConfig::from_lookup(lookup_from(&pairs)).unwrap_err();
    assert_eq!(err.missing, vec!["LIVEKIT_URL", "AD_SIP_TRUNK"]);
}

#[test]
fn defaults_apply_when_optional_vars_are_unset() {
    let cfg = OrchestratorConfig::from_lookup(lookup_from(&all_required())).unwrap();
    assert_eq!(cfg.gateway.room_prefix, "campaign");
    assert_eq!(cfg.batch_limit, 20);
    assert_eq!(cfg.calls_per_sec, 10.0);
    assert_eq!(cfg.retry_sweep_ms, 1000);
    assert_eq!(cfg.event_sweep_ms, 2000);
    assert_eq!(cfg.attempt_check_ms, 15000);
    assert_eq!(cfg.node_capacity, 50);
    assert_eq!(cfg.cluster_ttl_s, 10);
    assert_eq!(
        cfg.etcd_endpoints,
        ["http://kine-a:2379", "http://kine-b:2379"]
    );
    assert_eq!(cfg.worker, "orchestrator");
    assert_eq!(cfg.per_call_gap(), Duration::from_millis(100));
}

#[test]
fn the_node_name_must_be_one_kine_key_segment() {
    let mut pairs = all_required();
    pairs.push(("HOSTNAME", "pod/with slash"));
    let err = OrchestratorConfig::from_lookup(lookup_from(&pairs)).unwrap_err();
    assert!(err.invalid[0].starts_with("HOSTNAME="), "{:?}", err.invalid);
}
#[test]
fn invalid_numbers_are_rejected_not_ignored() {
    let mut pairs = all_required();
    pairs.push(("AD_CALLS_PER_SEC", "fast"));
    pairs.push(("AD_BATCH_LIMIT", "0"));
    pairs.push(("AD_RETRY_SWEEP_MS", "-5"));
    let err = OrchestratorConfig::from_lookup(lookup_from(&pairs)).unwrap_err();
    assert!(err.missing.is_empty());
    assert_eq!(err.invalid.len(), 3, "{:?}", err.invalid);
    assert!(err.invalid[0].starts_with("AD_BATCH_LIMIT="));
    assert!(err.invalid[1].starts_with("AD_CALLS_PER_SEC="));
    assert!(err.invalid[2].starts_with("AD_RETRY_SWEEP_MS="));
}

#[test]
fn the_caller_id_must_be_e164() {
    for good in ["+15551234567", "+447911123456"] {
        let mut pairs = all_required();
        pairs.push(("AD_SIP_CALLER_ID", good));
        let cfg = OrchestratorConfig::from_lookup(lookup_from(&pairs)).unwrap();
        assert_eq!(cfg.sip.caller_id.as_deref(), Some(good));
    }
    for bad in [
        "15551234567",
        "+0551234567",
        "+1555",
        "+1555abc4567",
        "agentdialer",
    ] {
        let mut pairs = all_required();
        pairs.push(("AD_SIP_CALLER_ID", bad));
        let err = OrchestratorConfig::from_lookup(lookup_from(&pairs)).unwrap_err();
        assert!(
            err.invalid[0].starts_with("AD_SIP_CALLER_ID="),
            "{bad}: {:?}",
            err.invalid
        );
    }
}

#[test]
fn the_sip_settings_have_defaults_and_are_checked() {
    let cfg = OrchestratorConfig::from_lookup(lookup_from(&all_required())).unwrap();
    assert_eq!(cfg.sip.trunk, "carrier.example:5060");
    assert_eq!(cfg.sip.bind, "0.0.0.0:5060".parse().unwrap());
    assert_eq!(cfg.sip.proxy, None);
    assert_eq!(cfg.sip.advertise_ip, None);
    assert!(cfg.sip.auth.is_none());

    let mut pairs = all_required();
    pairs.retain(|(k, _)| *k != "AD_SIP_TRUNK");
    pairs.push(("AD_SIP_TRUNK", "sip:carrier.example"));
    pairs.push(("AD_SIP_PROXY", "kamailio:5060"));
    pairs.push(("AD_SIP_BIND", "5060"));
    pairs.push(("AD_SIP_ADVERTISE_IP", "pod-ip"));
    pairs.push(("AD_SIP_TRUNK_USER", "alice"));
    let err = OrchestratorConfig::from_lookup(lookup_from(&pairs)).unwrap_err();
    let names: Vec<&str> = err
        .invalid
        .iter()
        .map(|i| i.split(['=', ' ']).next().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "AD_SIP_BIND",
            "AD_SIP_ADVERTISE_IP",
            "AD_SIP_TRUNK_USER",
            "AD_SIP_TRUNK"
        ],
        "{:?}",
        err.invalid
    );

    let mut pairs = all_required();
    pairs.push(("AD_SIP_PROXY", "kamailio:5060"));
    pairs.push(("AD_SIP_TRUNK_USER", "alice"));
    pairs.push(("AD_SIP_TRUNK_PASSWORD", "s3cret"));
    let cfg = OrchestratorConfig::from_lookup(lookup_from(&pairs)).unwrap();
    assert_eq!(cfg.sip.proxy.as_deref(), Some("kamailio:5060"));
    assert!(
        !format!("{cfg:?}").contains("s3cret"),
        "Debug hides the password"
    );
}
#[test]
fn migrate_requires_database_url_and_api_requires_nothing() {
    let err = check_required("migrate", MIGRATE, lookup_from(&[])).unwrap_err();
    assert_eq!(err.missing, vec!["DATABASE_URL"]);
    assert!(check_required("api", API, lookup_from(&[])).is_ok());
}

#[test]
fn the_api_needs_both_livekit_keys_or_neither() {
    let none = api_config(lookup_from(&[])).unwrap();
    assert_eq!(none.livekit_keys, None);
    assert_eq!(none.addr, "0.0.0.0:8080");

    let both = api_config(lookup_from(&[
        ("LIVEKIT_API_KEY", "key"),
        ("LIVEKIT_API_SECRET", "secret"),
        ("DATABASE_URL", "postgres://x"),
    ]))
    .unwrap();
    assert_eq!(both.livekit_keys, Some(("key".into(), "secret".into())));
    assert_eq!(both.database_url.as_deref(), Some("postgres://x"));

    let half = api_config(lookup_from(&[("LIVEKIT_API_KEY", "key")])).unwrap_err();
    assert_eq!(half.role, "api");
    assert!(half.invalid[0].contains("LIVEKIT_API_SECRET"), "{half}");
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

/// Every `agentdialer:e2e` service in the e2e compose files, as (file, service,
/// role, env names). The files use one layout: a service at two spaces, its
/// `command: ["<role>"]` and an `environment:` list of `- NAME=value` lines.
/// The agentdialer services of every compose file: `e2e/*.yml` and the
/// repository's `docker-compose*.yml` (the quickstart among them).
fn e2e_compose_services() -> Vec<(String, String, String, BTreeSet<String>)> {
    let mut out = Vec::new();
    let in_dir = |dir: PathBuf, keep: &dyn Fn(&str) -> bool| -> Vec<PathBuf> {
        std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.file_name().is_some_and(|n| keep(&n.to_string_lossy())))
            .collect()
    };
    let mut files = in_dir(repo_root().join("e2e"), &|n| n.ends_with(".yml"));
    files.extend(in_dir(repo_root(), &|n| {
        n.starts_with("docker-compose") && n.ends_with(".yml")
    }));
    files.sort();
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        let file = path.file_name().unwrap().to_string_lossy().to_string();
        let mut service: Option<(String, bool, Option<String>, BTreeSet<String>)> = None;
        let mut in_env = false;
        let mut flush = |s: Option<(String, bool, Option<String>, BTreeSet<String>)>| {
            if let Some((name, ours, Some(role), env)) = s {
                if ours {
                    out.push((file.clone(), name, role, env));
                }
            }
        };
        for line in text.lines() {
            let indent = line.len() - line.trim_start().len();
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            if indent == 2 && t.ends_with(':') {
                flush(service.take());
                service = Some((
                    t.trim_end_matches(':').to_string(),
                    false,
                    None,
                    BTreeSet::new(),
                ));
                in_env = false;
                continue;
            }
            if indent < 2 {
                flush(service.take());
                continue;
            }
            let Some((_, ours, role, env)) = service.as_mut() else {
                continue;
            };
            if indent == 4 {
                in_env = t == "environment:";
                if t.starts_with("image: agentdialer:") {
                    *ours = true;
                }
                if let Some(cmd) = t.strip_prefix("command:") {
                    *role = Some(cmd.trim().trim_matches(['[', ']', '"', ' ']).to_string());
                }
            } else if in_env && indent >= 6 {
                if let Some(entry) = t.strip_prefix("- ") {
                    env.insert(entry.split('=').next().unwrap_or("").to_string());
                }
            }
        }
        flush(service.take());
    }
    out
}

#[test]
fn e2e_compose_services_pass_exactly_the_role_contract() {
    let services = e2e_compose_services();
    assert!(
        services
            .iter()
            .filter(|(_, _, role, _)| role == "orchestrator")
            .count()
            >= 3,
        "expected the cluster compose to run three orchestrator nodes: {services:?}"
    );
    for (file, service, role, env) in &services {
        assert_env_matches_contract(&format!("{file} ({service})"), role, env);
    }
}

/// The quickstart stack (LiveKit Cloud SIP, Twilio) must send through the SIP
/// edge, answer the trunk's 407 challenge, and carry a caller ID.
#[test]
fn the_quickstart_node_passes_what_a_trunk_needs() {
    let services = e2e_compose_services();
    let nodes: Vec<_> = services
        .iter()
        .filter(|(file, _, role, _)| {
            file == "docker-compose.quickstart.yml" && role == "orchestrator"
        })
        .collect();
    assert!(
        !nodes.is_empty(),
        "docker-compose.quickstart.yml must run an orchestrator"
    );
    for (_, service, _, env) in nodes {
        for need in [
            "AD_SIP_TRUNK",
            "AD_SIP_PROXY",
            "AD_SIP_TRUNK_USER",
            "AD_SIP_TRUNK_PASSWORD",
            "AD_SIP_CALLER_ID",
        ] {
            assert!(
                env.contains(need),
                "docker-compose.quickstart.yml ({service}) must set {need}"
            );
        }
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

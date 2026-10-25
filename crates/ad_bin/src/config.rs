//! The environment contract of each `agentdialer` role.
//!
//! These lists are the single source of truth. Each role checks its list at
//! startup and reports every missing or invalid variable at once. The tests at
//! the end of this file check the Helm chart, the e2e scripts, and the README
//! against the same lists, so a launcher cannot drift from the binary.

use std::fmt;
use std::time::Duration;

use ad_gateway::GatewayConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    Required,
    Optional,
}

#[derive(Debug, Clone, Copy)]
pub struct EnvVar {
    pub name: &'static str,
    pub need: Need,
}

const fn req(name: &'static str) -> EnvVar {
    EnvVar {
        name,
        need: Need::Required,
    }
}

const fn opt(name: &'static str) -> EnvVar {
    EnvVar {
        name,
        need: Need::Optional,
    }
}

/// `agentdialer api`. Without `DATABASE_URL` the API uses an in-memory store.
/// The LiveKit key and secret verify LiveKit webhooks; set both or neither.
pub const API: &[EnvVar] = &[
    opt("AD_API_ADDR"),
    opt("DATABASE_URL"),
    opt("LIVEKIT_API_KEY"),
    opt("LIVEKIT_API_SECRET"),
    opt("RUST_LOG"),
];

/// `agentdialer migrate`.
pub const MIGRATE: &[EnvVar] = &[req("DATABASE_URL"), opt("RUST_LOG")];

/// `agentdialer orchestrator`.
pub const ORCHESTRATOR: &[EnvVar] = &[
    req("DATABASE_URL"),
    req("LIVEKIT_URL"),
    req("LIVEKIT_API_KEY"),
    req("LIVEKIT_API_SECRET"),
    opt("AD_DIAL_PROVIDER"),
    opt("AD_SIP_TRUNK"),
    opt("AD_SIP_PROXY"),
    opt("AD_SIP_BIND"),
    opt("AD_SIP_ADVERTISE_IP"),
    opt("AD_SIP_TRUNK_USER"),
    opt("AD_SIP_TRUNK_PASSWORD"),
    opt("AD_SIP_CALLER_ID"),
    opt("AD_MEDIA_PUBLIC_IP"),
    opt("AD_TWILIO_API_URL"),
    opt("AD_TWILIO_ACCOUNT_SID"),
    opt("AD_TWILIO_API_KEY"),
    opt("AD_TWILIO_API_SECRET"),
    opt("AD_TWILIO_SIP_URI"),
    req("AD_ETCD_ENDPOINTS"),
    opt("AD_ROOM_PREFIX"),
    opt("AD_BATCH_LIMIT"),
    opt("AD_CALLS_PER_SEC"),
    opt("AD_RETRY_SWEEP_MS"),
    opt("AD_EVENT_SWEEP_MS"),
    opt("AD_ATTEMPT_CHECK_MS"),
    opt("AD_NODE_CAPACITY"),
    opt("AD_CLUSTER_TTL_S"),
    opt("HOSTNAME"),
    opt("RUST_LOG"),
];

/// Missing and invalid variables for one role, reported together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub role: &'static str,
    pub missing: Vec<&'static str>,
    pub invalid: Vec<String>,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "agentdialer {}: configuration error.", self.role)?;
        if !self.missing.is_empty() {
            write!(
                f,
                " Missing required environment variables: {}.",
                self.missing.join(", ")
            )?;
        }
        if !self.invalid.is_empty() {
            write!(f, " Invalid values: {}.", self.invalid.join("; "))?;
        }
        write!(f, " See README.md, section \"Configuration\".")
    }
}

impl std::error::Error for ConfigError {}

/// A variable is missing when it is unset or holds only whitespace.
fn value(lookup: &impl Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    lookup(name).filter(|v| !v.trim().is_empty())
}

fn missing_required(
    vars: &[EnvVar],
    lookup: &impl Fn(&str) -> Option<String>,
) -> Vec<&'static str> {
    vars.iter()
        .filter(|v| v.need == Need::Required && value(lookup, v.name).is_none())
        .map(|v| v.name)
        .collect()
}

/// Check only the required variables of a role. For roles that read their
/// values themselves (`api`, `migrate`).
pub fn check_required(
    role: &'static str,
    vars: &[EnvVar],
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<(), ConfigError> {
    let missing = missing_required(vars, &lookup);
    if missing.is_empty() {
        Ok(())
    } else {
        Err(ConfigError {
            role,
            missing,
            invalid: Vec::new(),
        })
    }
}

/// The validated configuration of the api role.
pub fn api_config(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<ad_api::ServeConfig, ConfigError> {
    check_required("api", API, &lookup)?;
    let key = value(&lookup, "LIVEKIT_API_KEY");
    let secret = value(&lookup, "LIVEKIT_API_SECRET");
    let livekit_keys = match (key, secret) {
        (Some(k), Some(s)) => Some((k, s)),
        (None, None) => None,
        (k, _) => {
            let (set, unset) = if k.is_some() {
                ("LIVEKIT_API_KEY", "LIVEKIT_API_SECRET")
            } else {
                ("LIVEKIT_API_SECRET", "LIVEKIT_API_KEY")
            };
            return Err(ConfigError {
                role: "api",
                missing: Vec::new(),
                invalid: vec![format!(
                    "{set} is set but {unset} is not (set both or neither)"
                )],
            });
        }
    };
    Ok(ad_api::ServeConfig {
        addr: value(&lookup, "AD_API_ADDR").unwrap_or_else(|| "0.0.0.0:8080".into()),
        database_url: value(&lookup, "DATABASE_URL"),
        livekit_keys,
    })
}

/// Read the process environment.
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The SIP side of the orchestrator: the node carries its calls (PLAN.md
/// §0.0). `Debug` omits the trunk password.
#[derive(Clone)]
pub struct SipSettings {
    /// The carrier's host and port: the Request-URI is `sip:<number>@<trunk>`.
    pub trunk: String,
    /// The SIP edge (Kamailio), `host:port`. Every INVITE goes to it.
    pub proxy: Option<String>,
    /// The local SIP socket.
    pub bind: std::net::SocketAddr,
    /// The address in Via, Contact, and the SDP (the pod IP). Unset: the
    /// local address of the route to the proxy, or else to the trunk.
    pub advertise_ip: Option<std::net::IpAddr>,
    /// Digest credentials for the trunk.
    pub auth: Option<(String, String)>,
    /// The `public` media mode: the SDP names this address.
    pub media_public_ip: Option<std::net::IpAddr>,
    /// The E.164 caller ID in our From. Twilio requires a number of the
    /// account or a verified caller ID.
    pub caller_id: Option<String>,
}

impl fmt::Debug for SipSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SipSettings")
            .field("trunk", &self.trunk)
            .field("proxy", &self.proxy)
            .field("bind", &self.bind)
            .field("advertise_ip", &self.advertise_ip)
            .field("auth_user", &self.auth.as_ref().map(|(u, _)| u))
            .field("media_public_ip", &self.media_public_ip)
            .field("caller_id", &self.caller_id)
            .finish()
    }
}

/// The telephony provider of the node (`AD_DIAL_PROVIDER`).
#[derive(Debug, Clone)]
pub enum DialProvider {
    /// The node sends the INVITE to `AD_SIP_TRUNK`.
    SipTrunk,
    /// Twilio calls the lead, then sends an INVITE to the node.
    TwilioClickToCall(ad_gateway::TwilioVoice),
}

/// The validated configuration of the orchestrator role. `Debug` omits the
/// database URL and the LiveKit secret.
pub struct OrchestratorConfig {
    pub database_url: String,
    pub gateway: GatewayConfig,
    pub sip: SipSettings,
    pub provider: DialProvider,
    pub batch_limit: i64,
    pub calls_per_sec: f64,
    /// Interval of the sweep that dials leads that became due (a retry, a
    /// hold), settles ended attempts, and completes idle campaigns.
    pub retry_sweep_ms: u64,
    /// The event consumer reads the event table at least this often, also
    /// when a notification is lost.
    pub event_sweep_ms: u64,
    /// Interval of the reconcile that checks live attempts against LiveKit.
    /// It ends a call whose end event was lost.
    pub attempt_check_ms: u64,
    /// The kine (etcd API) endpoints.
    pub etcd_endpoints: Vec<String>,
    /// The most calls this node runs at one time.
    pub node_capacity: u32,
    /// The TTL of the node and leader keys in kine.
    pub cluster_ttl_s: u64,
    pub worker: String,
}

impl OrchestratorConfig {
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut missing = missing_required(ORCHESTRATOR, &lookup);
        let mut invalid = Vec::new();
        let provider_name =
            value(&lookup, "AD_DIAL_PROVIDER").unwrap_or_else(|| "sip_trunk".into());
        let needs: &[&'static str] = match provider_name.trim() {
            "sip_trunk" => &["AD_SIP_TRUNK"],
            "twilio_click_to_call" => &[
                "AD_TWILIO_ACCOUNT_SID",
                "AD_TWILIO_API_KEY",
                "AD_TWILIO_API_SECRET",
                "AD_TWILIO_SIP_URI",
                "AD_SIP_CALLER_ID",
            ],
            other => {
                invalid.push(format!(
                    "AD_DIAL_PROVIDER={other:?} (expected one of: {})",
                    ad_gateway::Telephony::NAMES.join(", ")
                ));
                &[]
            }
        };
        missing.extend(needs.iter().filter(|n| value(&lookup, n).is_none()));
        missing.sort_by_key(|m| ORCHESTRATOR.iter().position(|v| v.name == *m));
        let click_to_call = provider_name.trim() == "twilio_click_to_call";
        if click_to_call
            && value(&lookup, "AD_SIP_PROXY").is_none()
            && value(&lookup, "AD_SIP_ADVERTISE_IP").is_none()
        {
            invalid.push(
                "AD_DIAL_PROVIDER=twilio_click_to_call needs AD_SIP_PROXY or \
                 AD_SIP_ADVERTISE_IP (the address that Twilio's INVITE reaches)"
                    .into(),
            );
        }
        let twilio_url =
            value(&lookup, "AD_TWILIO_API_URL").unwrap_or_else(|| "https://api.twilio.com".into());
        if !(twilio_url.starts_with("https://") || twilio_url.starts_with("http://")) {
            invalid.push(format!(
                "AD_TWILIO_API_URL={twilio_url:?} (expected an http or https URL)"
            ));
        }
        if let Some(sid) = value(&lookup, "AD_TWILIO_ACCOUNT_SID") {
            if !sid.trim().starts_with("AC") {
                invalid.push(format!(
                    "AD_TWILIO_ACCOUNT_SID={sid:?} (expected an account SID, AC...)"
                ));
            }
        }
        if let Some(uri) = value(&lookup, "AD_TWILIO_SIP_URI") {
            if !uri.trim().starts_with("sip:") || uri.contains(char::is_whitespace) {
                invalid.push(format!(
                    "AD_TWILIO_SIP_URI={uri:?} (expected a SIP URI like sip:agentdialer@edge.example.com)"
                ));
            }
        }

        let batch_limit = positive(&lookup, "AD_BATCH_LIMIT", 20i64, &mut invalid);
        let calls_per_sec = positive(&lookup, "AD_CALLS_PER_SEC", 10.0f64, &mut invalid);
        let retry_sweep_ms = positive(&lookup, "AD_RETRY_SWEEP_MS", 1000u64, &mut invalid);
        let event_sweep_ms = positive(&lookup, "AD_EVENT_SWEEP_MS", 2000u64, &mut invalid);
        let attempt_check_ms = positive(&lookup, "AD_ATTEMPT_CHECK_MS", 15000u64, &mut invalid);
        let node_capacity = positive(&lookup, "AD_NODE_CAPACITY", 50u32, &mut invalid);
        let cluster_ttl_s = positive(&lookup, "AD_CLUSTER_TTL_S", 10u64, &mut invalid);
        let worker = value(&lookup, "HOSTNAME").unwrap_or_else(|| "orchestrator".into());
        if !ad_cluster::keys::valid_node_name(&worker) {
            invalid.push(format!(
                "HOSTNAME={worker:?} (a node name: letters, digits, '-', '_', '.')"
            ));
        }
        let bind = parsed(
            &lookup,
            "AD_SIP_BIND",
            "an address like 0.0.0.0:5060",
            &mut invalid,
        )
        .unwrap_or_else(|| ([0, 0, 0, 0], 5060).into());
        let advertise_ip = parsed(
            &lookup,
            "AD_SIP_ADVERTISE_IP",
            "an IP address",
            &mut invalid,
        );
        let media_public_ip = parsed(&lookup, "AD_MEDIA_PUBLIC_IP", "an IP address", &mut invalid);
        let caller_id = value(&lookup, "AD_SIP_CALLER_ID").map(|v| v.trim().to_string());
        if let Some(c) = &caller_id {
            if !is_e164(c) {
                invalid.push(format!(
                    "AD_SIP_CALLER_ID={c:?} (expected an E.164 number like +15551234567)"
                ));
            }
        }
        let auth = match (
            value(&lookup, "AD_SIP_TRUNK_USER"),
            value(&lookup, "AD_SIP_TRUNK_PASSWORD"),
        ) {
            (Some(u), Some(p)) => Some((u, p)),
            (None, None) => None,
            _ => {
                invalid.push(
                    "AD_SIP_TRUNK_USER and AD_SIP_TRUNK_PASSWORD (set both or neither)".into(),
                );
                None
            }
        };
        for name in ["AD_SIP_TRUNK", "AD_SIP_PROXY"] {
            if let Some(v) = value(&lookup, name) {
                let uri = v.starts_with("sip:") || v.starts_with("sips:");
                if uri || v.contains('@') || v.contains(char::is_whitespace) {
                    invalid.push(format!("{name}={v:?} (expected host or host:port)"));
                }
            }
        }

        if !missing.is_empty() || !invalid.is_empty() {
            return Err(ConfigError {
                role: "orchestrator",
                missing,
                invalid,
            });
        }

        // Every required value is present, as checked above.
        let get = |name: &str| value(&lookup, name).unwrap_or_default().trim().to_string();
        let provider = if click_to_call {
            DialProvider::TwilioClickToCall(ad_gateway::TwilioVoice {
                api_url: twilio_url,
                account_sid: get("AD_TWILIO_ACCOUNT_SID"),
                key_sid: get("AD_TWILIO_API_KEY"),
                key_secret: get("AD_TWILIO_API_SECRET"),
                sip_uri: get("AD_TWILIO_SIP_URI"),
                caller_id: caller_id.clone().unwrap_or_default(),
            })
        } else {
            DialProvider::SipTrunk
        };
        Ok(Self {
            provider,
            database_url: get("DATABASE_URL"),
            gateway: GatewayConfig {
                livekit_url: get("LIVEKIT_URL"),
                api_key: get("LIVEKIT_API_KEY"),
                api_secret: get("LIVEKIT_API_SECRET"),
                room_prefix: value(&lookup, "AD_ROOM_PREFIX").unwrap_or_else(|| "campaign".into()),
            },
            sip: SipSettings {
                trunk: get("AD_SIP_TRUNK"),
                proxy: value(&lookup, "AD_SIP_PROXY"),
                bind,
                advertise_ip,
                auth,
                media_public_ip,
                caller_id,
            },
            batch_limit,
            calls_per_sec,
            retry_sweep_ms,
            event_sweep_ms,
            attempt_check_ms,
            etcd_endpoints: get("AD_ETCD_ENDPOINTS")
                .split(',')
                .map(|e| e.trim().to_string())
                .filter(|e| !e.is_empty())
                .collect(),
            node_capacity,
            cluster_ttl_s,
            worker,
        })
    }

    /// The minimum gap between two call originations.
    pub fn per_call_gap(&self) -> Duration {
        Duration::from_millis((1000.0 / self.calls_per_sec).max(1.0) as u64)
    }
}

impl fmt::Debug for OrchestratorConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OrchestratorConfig")
            .field("livekit_url", &self.gateway.livekit_url)
            .field("sip", &self.sip)
            .field("provider", &self.provider)
            .field("room_prefix", &self.gateway.room_prefix)
            .field("batch_limit", &self.batch_limit)
            .field("calls_per_sec", &self.calls_per_sec)
            .field("retry_sweep_ms", &self.retry_sweep_ms)
            .field("event_sweep_ms", &self.event_sweep_ms)
            .field("attempt_check_ms", &self.attempt_check_ms)
            .field("etcd_endpoints", &self.etcd_endpoints)
            .field("node_capacity", &self.node_capacity)
            .field("cluster_ttl_s", &self.cluster_ttl_s)
            .field("worker", &self.worker)
            .finish_non_exhaustive()
    }
}

/// `+`, then 7 to 15 digits, the first not 0.
fn is_e164(s: &str) -> bool {
    let Some(digits) = s.strip_prefix('+') else {
        return false;
    };
    (7..=15).contains(&digits.len())
        && digits.chars().all(|c| c.is_ascii_digit())
        && !digits.starts_with('0')
}

/// Parse an optional value, or record why it is invalid.
fn parsed<T: std::str::FromStr>(
    lookup: &impl Fn(&str) -> Option<String>,
    name: &str,
    expected: &str,
    invalid: &mut Vec<String>,
) -> Option<T> {
    let raw = value(lookup, name)?;
    match raw.trim().parse::<T>() {
        Ok(v) => Some(v),
        Err(_) => {
            invalid.push(format!("{name}={raw:?} (expected {expected})"));
            None
        }
    }
}

/// Parse an optional positive number, or record why it is invalid.
fn positive<T>(
    lookup: &impl Fn(&str) -> Option<String>,
    name: &str,
    default: T,
    invalid: &mut Vec<String>,
) -> T
where
    T: std::str::FromStr + PartialOrd + Default + Copy,
{
    let Some(raw) = value(lookup, name) else {
        return default;
    };
    match raw.trim().parse::<T>() {
        Ok(v) if v > T::default() => v,
        _ => {
            invalid.push(format!("{name}={raw:?} (expected a number greater than 0)"));
            default
        }
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;

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
pub const API: &[EnvVar] = &[opt("AD_API_ADDR"), opt("DATABASE_URL"), opt("RUST_LOG")];

/// `agentdialer migrate`.
pub const MIGRATE: &[EnvVar] = &[req("DATABASE_URL"), opt("RUST_LOG")];

/// `agentdialer orchestrator`.
pub const ORCHESTRATOR: &[EnvVar] = &[
    req("DATABASE_URL"),
    req("REDIS_URL"),
    req("LIVEKIT_URL"),
    req("LIVEKIT_API_KEY"),
    req("LIVEKIT_API_SECRET"),
    req("AD_SIP_TRUNK_ID"),
    opt("AD_ROOM_PREFIX"),
    opt("AD_BATCH_LIMIT"),
    opt("AD_CALLS_PER_SEC"),
    opt("AD_TIMER_POLL_MS"),
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

/// Read the process environment.
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The validated configuration of the orchestrator role. `Debug` omits the
/// database URL and the LiveKit secret.
pub struct OrchestratorConfig {
    pub database_url: String,
    pub redis_url: String,
    pub gateway: GatewayConfig,
    pub batch_limit: i64,
    pub calls_per_sec: f64,
    pub timer_poll_ms: u64,
    pub worker: String,
}

impl OrchestratorConfig {
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let missing = missing_required(ORCHESTRATOR, &lookup);
        let mut invalid = Vec::new();

        let batch_limit = positive(&lookup, "AD_BATCH_LIMIT", 20i64, &mut invalid);
        let calls_per_sec = positive(&lookup, "AD_CALLS_PER_SEC", 10.0f64, &mut invalid);
        let timer_poll_ms = positive(&lookup, "AD_TIMER_POLL_MS", 1000u64, &mut invalid);

        if !missing.is_empty() || !invalid.is_empty() {
            return Err(ConfigError {
                role: "orchestrator",
                missing,
                invalid,
            });
        }

        // Every required value is present, as checked above.
        let get = |name: &str| value(&lookup, name).unwrap_or_default();
        Ok(Self {
            database_url: get("DATABASE_URL"),
            redis_url: get("REDIS_URL"),
            gateway: GatewayConfig {
                livekit_url: get("LIVEKIT_URL"),
                api_key: get("LIVEKIT_API_KEY"),
                api_secret: get("LIVEKIT_API_SECRET"),
                sip_trunk_id: get("AD_SIP_TRUNK_ID"),
                room_prefix: value(&lookup, "AD_ROOM_PREFIX").unwrap_or_else(|| "campaign".into()),
            },
            batch_limit,
            calls_per_sec,
            timer_poll_ms,
            worker: value(&lookup, "HOSTNAME").unwrap_or_else(|| "orchestrator".into()),
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
            .field("sip_trunk_id", &self.gateway.sip_trunk_id)
            .field("room_prefix", &self.gateway.room_prefix)
            .field("batch_limit", &self.batch_limit)
            .field("calls_per_sec", &self.calls_per_sec)
            .field("timer_poll_ms", &self.timer_poll_ms)
            .field("worker", &self.worker)
            .finish_non_exhaustive()
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

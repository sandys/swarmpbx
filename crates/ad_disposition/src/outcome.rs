//! The agent's report of a call, and its check against the catalog.

use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::callback::local_to_utc;
use crate::catalog::{Catalog, Disposition};

/// The version of the report that this dialer reads.
pub const SCHEMA_VERSION: u32 = 1;

/// A callback must be at least this far in the future.
pub const MIN_CALLBACK_LEAD: Duration = Duration::minutes(1);
/// A callback must be at most this far in the future.
pub const MAX_CALLBACK_LEAD: Duration = Duration::days(90);

const MAX_SUMMARY: usize = 4000;
const MAX_DISCLOSURES: usize = 20;
const MAX_TEXT: usize = 200;

/// Who answered, as the agent heard it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnsweredBy {
    #[default]
    Unknown,
    Human,
    Machine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DncScope {
    /// Every campaign.
    System,
    /// The campaign of the call.
    Campaign,
}

impl DncScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            DncScope::System => "system",
            DncScope::Campaign => "campaign",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "system" => Some(DncScope::System),
            "campaign" => Some(DncScope::Campaign),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackRequest {
    /// The local time, `YYYY-MM-DDTHH:MM` or `YYYY-MM-DDTHH:MM:SS`.
    pub at: String,
    /// The IANA zone. Without it, the zone of the lead.
    pub zone: Option<String>,
}

/// The report as the agent sends it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub schema_version: u32,
    pub disposition: Option<String>,
    pub answered_by: AnsweredBy,
    pub callback: Option<CallbackRequest>,
    /// The callee asked not to be called again, with the scope. `Some(None)`
    /// asks for the system list.
    pub dnc: Option<Option<String>>,
    /// The number or the SIP URI the agent asked to transfer to. The dialer
    /// records it; it does not transfer calls yet.
    pub transfer_to: Option<String>,
    pub summary: Option<String>,
    /// The disclosures the agent spoke, for example `recording_notice`.
    pub disclosures: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Callback {
    pub at_utc: DateTime<Utc>,
    pub local: NaiveDateTime,
    pub zone: String,
}

/// A report that passed the check. The dialer stores it with the attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentOutcome {
    pub schema_version: u32,
    /// The disposition with its flags at the time of the report.
    pub disposition: Option<Disposition>,
    pub answered_by: AnsweredBy,
    pub callback: Option<Callback>,
    pub dnc: Option<DncScope>,
    pub transfer_to: Option<String>,
    pub summary: Option<String>,
    pub disclosures: Vec<String>,
}

impl AgentOutcome {
    /// A machine answered: the flag of the disposition, or the agent's
    /// answer class when it gives no disposition.
    pub fn machine(&self) -> bool {
        match &self.disposition {
            Some(d) => d.flags.answering_machine,
            None => self.answered_by == AnsweredBy::Machine,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Rejection {
    #[error("schema_version {0} is not supported (this dialer reads {SCHEMA_VERSION})")]
    Version(u32),
    #[error("disposition '{0}' is not in the catalog of the campaign")]
    UnknownDisposition(String),
    #[error("disposition '{0}' needs a callback time")]
    CallbackMissing(String),
    #[error("a callback needs a disposition with the scheduled_callback flag, not '{0}'")]
    CallbackNotAllowed(String),
    #[error("a callback and a DNC request exclude each other")]
    CallbackAndDnc,
    #[error("callback time '{0}': use YYYY-MM-DDTHH:MM in the local time")]
    CallbackTime(String),
    #[error("callback zone '{0}' is not an IANA zone")]
    CallbackZone(String),
    #[error("a callback needs a zone, and the lead has none")]
    CallbackNoZone,
    #[error("the callback must be between 1 minute and 90 days from now")]
    CallbackRange,
    #[error("DNC scope '{0}': use system or campaign")]
    DncScope(String),
    #[error("{0} is too long")]
    TooLong(&'static str),
}

/// Check a report against the catalog of the campaign at `now`. `lead_zone`
/// is the IANA zone of the lead.
pub fn check(
    report: &Report,
    catalog: &Catalog,
    lead_zone: Option<&str>,
    now: DateTime<Utc>,
) -> Result<AgentOutcome, Rejection> {
    if report.schema_version != SCHEMA_VERSION {
        return Err(Rejection::Version(report.schema_version));
    }
    let disposition = match report.disposition.as_deref() {
        Some(code) => Some(
            catalog
                .get(code)
                .ok_or_else(|| Rejection::UnknownDisposition(code.into()))?,
        ),
        None => None,
    };
    let flags = disposition.as_ref().map(|d| d.flags).unwrap_or_default();

    let dnc = match &report.dnc {
        Some(Some(scope)) => {
            Some(DncScope::parse(scope).ok_or_else(|| Rejection::DncScope(scope.clone()))?)
        }
        Some(None) => Some(DncScope::System),
        None if flags.dnc => Some(DncScope::System),
        None => None,
    };

    let callback = match &report.callback {
        Some(req) => {
            if let Some(d) = disposition.as_ref().filter(|d| !d.flags.scheduled_callback) {
                return Err(Rejection::CallbackNotAllowed(d.code.clone()));
            }
            if dnc.is_some() {
                return Err(Rejection::CallbackAndDnc);
            }
            Some(callback(req, lead_zone, now)?)
        }
        None if flags.scheduled_callback => {
            let code = disposition.as_ref().map(|d| d.code.clone());
            return Err(Rejection::CallbackMissing(code.unwrap_or_default()));
        }
        None => None,
    };

    let text = |s: &Option<String>, max: usize, what: &'static str| match s {
        Some(s) if s.chars().count() > max => Err(Rejection::TooLong(what)),
        Some(s) if s.trim().is_empty() => Ok(None),
        Some(s) => Ok(Some(s.trim().to_string())),
        None => Ok(None),
    };
    if report.disclosures.len() > MAX_DISCLOSURES
        || report
            .disclosures
            .iter()
            .any(|d| d.chars().count() > MAX_TEXT)
    {
        return Err(Rejection::TooLong("disclosures"));
    }
    Ok(AgentOutcome {
        schema_version: SCHEMA_VERSION,
        disposition,
        answered_by: report.answered_by,
        callback,
        dnc,
        transfer_to: text(&report.transfer_to, MAX_TEXT, "transfer_to")?,
        summary: text(&report.summary, MAX_SUMMARY, "summary")?,
        disclosures: report.disclosures.clone(),
    })
}

fn callback(
    req: &CallbackRequest,
    lead_zone: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Callback, Rejection> {
    let zone_name = req
        .zone
        .as_deref()
        .or(lead_zone)
        .ok_or(Rejection::CallbackNoZone)?;
    let zone: Tz = zone_name
        .parse()
        .map_err(|_| Rejection::CallbackZone(zone_name.into()))?;
    let local = ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"]
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(req.at.trim(), f).ok())
        .ok_or_else(|| Rejection::CallbackTime(req.at.clone()))?;
    let at_utc =
        local_to_utc(local, zone).ok_or_else(|| Rejection::CallbackTime(req.at.clone()))?;
    if at_utc < now + MIN_CALLBACK_LEAD || at_utc > now + MAX_CALLBACK_LEAD {
        return Err(Rejection::CallbackRange);
    }
    Ok(Callback {
        at_utc,
        local,
        zone: zone_name.into(),
    })
}

#[cfg(test)]
#[path = "outcome_tests.rs"]
mod outcome_tests;

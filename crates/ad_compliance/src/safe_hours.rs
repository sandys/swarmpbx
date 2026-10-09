//! Safe-hours / calling-window gate (TCPA-style).
//!
//! Mirrors VICIdial's `vicidial_call_times` model: a window set has a per-day
//! start/stop in `HHMM` (local), optional holidays, and we check the lead's
//! *local* time against it. Fails closed: unknown tz or malformed window => deny.

use chrono::{Datelike, NaiveDate, TimeZone, Timelike, Weekday};
use chrono_tz::Tz;

use crate::{Decision, DenyReason};
use ad_core::Clock;

/// Minutes-since-midnight local time, 0..=1440. `start==stop` means "closed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DayWindow {
    pub start: u16, // inclusive, minutes since local midnight
    pub stop: u16,  // exclusive
}

impl DayWindow {
    /// Closed window (no calling). VICIdial uses start==stop==0 for this.
    pub const CLOSED: DayWindow = DayWindow { start: 0, stop: 0 };

    /// Build from VICIdial-style HHMM integers (e.g. 900 = 09:00, 2100 = 21:00).
    /// Returns None if the HHMM is not a valid clock time — caller fails closed.
    pub fn from_hhmm(start_hhmm: u16, stop_hhmm: u16) -> Option<DayWindow> {
        Some(DayWindow {
            start: hhmm_to_minutes(start_hhmm)?,
            stop: hhmm_to_minutes(stop_hhmm)?,
        })
    }

    fn is_closed(&self) -> bool {
        self.start == self.stop
    }

    fn contains(&self, minutes: u16) -> bool {
        if self.is_closed() {
            return false;
        }
        if self.start <= self.stop {
            minutes >= self.start && minutes < self.stop
        } else {
            // Window wraps past midnight (e.g. 2000..0200).
            minutes >= self.start || minutes < self.stop
        }
    }
}

fn hhmm_to_minutes(hhmm: u16) -> Option<u16> {
    let h = hhmm / 100;
    let m = hhmm % 100;
    if h > 24 || m >= 60 {
        return None;
    }
    let mins = h * 60 + m;
    if mins > 1440 {
        None
    } else {
        Some(mins)
    }
}

/// A full weekly calling-window set + holidays, in the window's own timezone
/// semantics (the lead's local tz is applied at check time).
#[derive(Debug, Clone)]
pub struct CallWindowSet {
    /// Per-weekday windows, indexed Mon=0..Sun=6.
    pub by_weekday: [DayWindow; 7],
    /// Dates on which no calling is allowed regardless of weekday window.
    pub holidays: Vec<NaiveDate>,
}

impl CallWindowSet {
    /// A uniform window every day (common case: 9am–9pm).
    pub fn uniform(start_hhmm: u16, stop_hhmm: u16) -> Option<CallWindowSet> {
        let w = DayWindow::from_hhmm(start_hhmm, stop_hhmm)?;
        Some(CallWindowSet {
            by_weekday: [w; 7],
            holidays: Vec::new(),
        })
    }

    fn window_for(&self, weekday: Weekday) -> DayWindow {
        self.by_weekday[weekday.num_days_from_monday() as usize]
    }
}

/// What we know about a lead for the purpose of local-time resolution.
/// `timezone` is an IANA name (e.g. "America/New_York", "Asia/Kolkata").
/// If absent/unparseable, the engine fails closed.
#[derive(Debug, Clone, Default)]
pub struct LeadLocation {
    pub timezone: Option<String>,
}

/// Resolve the lead's IANA timezone, or None (=> fail closed).
fn resolve_tz(loc: &LeadLocation) -> Option<Tz> {
    loc.timezone.as_deref()?.parse::<Tz>().ok()
}

/// The safe-hours decision. Pure: depends only on (clock, location, windows).
pub fn check(clock: &dyn Clock, loc: &LeadLocation, windows: &CallWindowSet) -> Decision {
    let Some(tz) = resolve_tz(loc) else {
        return Decision::Denied(DenyReason::UnknownLocalTime);
    };

    // Convert "now" to the lead's local wall-clock time.
    let now_utc = clock.now_utc();
    let local = now_utc.with_timezone(&tz);
    let local_date = local.date_naive();

    // Holiday?
    if windows.holidays.contains(&local_date) {
        return Decision::Denied(DenyReason::Holiday);
    }

    let minutes = (local.hour() * 60 + local.minute()) as u16;
    let window = windows.window_for(local.weekday());

    if window.contains(minutes) {
        Decision::Allowed
    } else {
        Decision::Denied(DenyReason::OutsideCallingWindow)
    }
}

/// Helper for callers that build a tz-aware "now" for display/logging.
pub fn local_now(clock: &dyn Clock, loc: &LeadLocation) -> Option<chrono::DateTime<Tz>> {
    let tz = resolve_tz(loc)?;
    Some(clock.now_utc().with_timezone(&tz))
}

/// Construct a UTC instant from parts — a convenience for building fixtures and
/// for callers that need a known reference point.
pub fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> chrono::DateTime<chrono::Utc> {
    chrono::Utc
        .with_ymd_and_hms(y, mo, d, h, mi, 0)
        .single()
        .expect("valid utc parts")
}

//! The UTC time of a local callback time.
//!
//! The rule for the daylight saving changes (QuickVoice `campaign-time.ts`
//! makes the same choice):
//! - A local time in the gap of the spring change does not occur. The
//!   callback goes to the first local minute after the gap.
//! - A local time in the overlap of the autumn change occurs two times. The
//!   callback goes to the earlier one.

use chrono::{DateTime, Duration, LocalResult, NaiveDateTime, TimeZone, Timelike, Utc};
use chrono_tz::Tz;

/// The longest gap that the rule steps over. Real gaps are 30 or 60 minutes.
const MAX_GAP_MIN: i64 = 24 * 60;

/// The UTC time of `local` in `zone`, with the rule of this module.
pub fn local_to_utc(local: NaiveDateTime, zone: Tz) -> Option<DateTime<Utc>> {
    match zone.from_local_datetime(&local) {
        LocalResult::Single(t) => Some(t.with_timezone(&Utc)),
        LocalResult::Ambiguous(a, b) => Some(a.min(b).with_timezone(&Utc)),
        LocalResult::None => {
            let minute = local.with_second(0)?.with_nanosecond(0)?;
            (1..=MAX_GAP_MIN).find_map(|n| {
                match zone.from_local_datetime(&(minute + Duration::minutes(n))) {
                    LocalResult::Single(t) => Some(t.with_timezone(&Utc)),
                    LocalResult::Ambiguous(a, b) => Some(a.min(b).with_timezone(&Utc)),
                    LocalResult::None => None,
                }
            })
        }
    }
}

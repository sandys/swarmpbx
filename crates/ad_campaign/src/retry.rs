//! The retry settings of a campaign: how long a lead waits after each call
//! result, how many calls a lead gets, and how soon a callback can be. The
//! API changes them at run time; the next settlement uses them.

use serde::{Deserialize, Serialize};

/// The longest wait before a retry: 90 days.
pub const MAX_WAIT_S: u32 = 90 * 86_400;
/// The most calls that a campaign can give one lead.
pub const MAX_CALLS: u32 = 100;
/// The highest callback floor: 1 day.
pub const MAX_CALLBACK_MIN_S: u32 = 86_400;

/// The stored JSON holds only the changed keys. A key that is not there has
/// its default value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Retry {
    /// The wait after a busy signal.
    pub busy_s: u32,
    /// The wait after a call that nobody answered.
    pub no_answer_s: u32,
    /// The wait after the agent reported an answering machine.
    pub voicemail_s: u32,
    /// The wait after a call that the carrier refused.
    pub failed_s: u32,
    /// The wait after a setup error or a result that is not certain.
    pub error_s: u32,
    /// The wait after an abandoned call.
    pub abandoned_s: u32,
    /// A lead gets no retry after this many calls.
    pub max_calls: u32,
    /// An agent's callback must be at least this far in the future.
    pub callback_min_s: u32,
}

impl Default for Retry {
    fn default() -> Self {
        Retry {
            busy_s: 300,
            no_answer_s: 1800,
            voicemail_s: 3600,
            failed_s: 900,
            error_s: 900,
            abandoned_s: 900,
            max_calls: 3,
            callback_min_s: 60,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("retry: {0}")]
pub struct RetryError(pub String);

impl Retry {
    /// These settings with the keys of `change` (a JSON object) replaced.
    /// Refuses an unknown key, a value of the wrong type, or a value out of
    /// range.
    pub fn changed(&self, change: &serde_json::Value) -> Result<Retry, RetryError> {
        let serde_json::Value::Object(change) = change else {
            return Err(RetryError("give an object of settings".into()));
        };
        let serde_json::Value::Object(mut all) =
            serde_json::to_value(self).map_err(|e| RetryError(e.to_string()))?
        else {
            return Err(RetryError("the settings are not an object".into()));
        };
        all.extend(change.clone());
        let retry: Retry = serde_json::from_value(serde_json::Value::Object(all))
            .map_err(|e| RetryError(e.to_string()))?;
        retry.check()?;
        Ok(retry)
    }

    /// Each value must be in its range.
    pub fn check(&self) -> Result<(), RetryError> {
        let waits = [
            ("busy_s", self.busy_s),
            ("no_answer_s", self.no_answer_s),
            ("voicemail_s", self.voicemail_s),
            ("failed_s", self.failed_s),
            ("error_s", self.error_s),
            ("abandoned_s", self.abandoned_s),
        ];
        for (name, value) in waits {
            in_range(name, value, MAX_WAIT_S)?;
        }
        in_range("max_calls", self.max_calls, MAX_CALLS)?;
        in_range("callback_min_s", self.callback_min_s, MAX_CALLBACK_MIN_S)
    }
}

fn in_range(name: &str, value: u32, max: u32) -> Result<(), RetryError> {
    if (1..=max).contains(&value) {
        Ok(())
    } else {
        Err(RetryError(format!(
            "{name} must be from 1 to {max} (got {value})"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_change_replaces_only_its_keys() {
        let r = Retry::default()
            .changed(&json!({"busy_s": 5, "callback_min_s": 5}))
            .unwrap();
        assert_eq!(
            r,
            Retry {
                busy_s: 5,
                callback_min_s: 5,
                ..Retry::default()
            }
        );
        let again = r.changed(&json!({"max_calls": 7})).unwrap();
        assert_eq!((again.busy_s, again.max_calls), (5, 7));
    }

    #[test]
    fn a_change_refuses_bad_keys_and_values() {
        let d = Retry::default();
        for bad in [
            json!({"busy": 5}),
            json!({"busy_s": 0}),
            json!({"busy_s": -1}),
            json!({"busy_s": "5"}),
            json!({"busy_s": null}),
            json!({"no_answer_s": MAX_WAIT_S + 1}),
            json!({"max_calls": 0}),
            json!({"max_calls": MAX_CALLS + 1}),
            json!({"callback_min_s": MAX_CALLBACK_MIN_S + 1}),
            json!([1]),
        ] {
            assert!(d.changed(&bad).is_err(), "{bad} was taken");
        }
        assert_eq!(d.changed(&json!({})).unwrap(), d);
        assert!(d
            .changed(&json!({"no_answer_s": MAX_WAIT_S, "max_calls": MAX_CALLS}))
            .is_ok());
    }

    #[test]
    fn stored_keys_that_are_not_there_have_the_default() {
        let r: Retry = serde_json::from_value(json!({"busy_s": 9})).unwrap();
        assert_eq!(
            r,
            Retry {
                busy_s: 9,
                ..Retry::default()
            }
        );
        assert!(serde_json::from_value::<Retry>(json!({"what": 1})).is_err());
    }
}

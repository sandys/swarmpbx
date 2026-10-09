//! Typed identifiers. Newtypes over integers so a LeadId can never be passed
//! where a CampaignId is expected.

use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub i64);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl From<i64> for $name {
            fn from(v: i64) -> Self {
                Self(v)
            }
        }
    };
}

id_type!(OrgId);
id_type!(CampaignId);
id_type!(LeadId);
id_type!(RunId);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_matches_inner() {
        assert_eq!(CampaignId(42).to_string(), "42");
        assert_eq!(LeadId(-1).to_string(), "-1");
    }

    #[test]
    fn from_i64_constructs() {
        let c: CampaignId = 7i64.into();
        assert_eq!(c, CampaignId(7));
    }

    #[test]
    fn serde_is_transparent() {
        // `#[serde(transparent)]` must serialize as the bare integer, so ids
        // interoperate cleanly in JSON APIs and DB rows.
        let j = serde_json::to_string(&LeadId(123)).unwrap();
        assert_eq!(j, "123");
        let back: LeadId = serde_json::from_str("123").unwrap();
        assert_eq!(back, LeadId(123));
    }

    #[test]
    fn distinct_types_are_ordered_and_hashable() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(OrgId(1));
        set.insert(OrgId(1));
        set.insert(OrgId(2));
        assert_eq!(set.len(), 2);
        assert!(CampaignId(1) < CampaignId(2));
    }
}

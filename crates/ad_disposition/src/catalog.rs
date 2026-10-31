//! The disposition catalog. Each code carries the VICIdial flags. The flags,
//! not the code, decide what the dialer does with the lead.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Flags {
    pub human_answered: bool,
    pub sale: bool,
    /// The callee asked not to be called again. The dialer adds the number
    /// to the DNC list.
    pub dnc: bool,
    pub customer_contact: bool,
    pub not_interested: bool,
    pub unworkable: bool,
    /// The outcome must carry a callback time.
    pub scheduled_callback: bool,
    pub completed: bool,
    /// A machine answered. The dialer tries the lead again later.
    pub answering_machine: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Disposition {
    pub code: String,
    pub name: String,
    #[serde(default)]
    pub flags: Flags,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    #[error("code '{0}': use 1 to 16 characters A-Z, 0-9, or _")]
    Code(String),
    #[error("code '{0}' occurs two times")]
    Duplicate(String),
    #[error("code '{0}' is a system code")]
    System(String),
    #[error("code '{0}': the name is empty or longer than 64 characters")]
    Name(String),
    #[error("code '{code}': {why}")]
    Flags { code: String, why: &'static str },
}

fn d(code: &str, name: &str, flags: Flags) -> Disposition {
    Disposition {
        code: code.into(),
        name: name.into(),
        flags,
    }
}

/// The codes of every campaign.
pub fn system() -> Vec<Disposition> {
    let talked = Flags {
        human_answered: true,
        customer_contact: true,
        ..Flags::default()
    };
    vec![
        d(
            "SALE",
            "Sale",
            Flags {
                sale: true,
                completed: true,
                ..talked
            },
        ),
        d(
            "NI",
            "Not interested",
            Flags {
                not_interested: true,
                completed: true,
                ..talked
            },
        ),
        d(
            "CALLBK",
            "Callback",
            Flags {
                scheduled_callback: true,
                ..talked
            },
        ),
        d(
            "DNC",
            "Do not call",
            Flags {
                dnc: true,
                completed: true,
                ..talked
            },
        ),
        d(
            "AM",
            "Answering machine",
            Flags {
                answering_machine: true,
                ..Flags::default()
            },
        ),
        d(
            "WN",
            "Wrong number",
            Flags {
                human_answered: true,
                unworkable: true,
                completed: true,
                ..Flags::default()
            },
        ),
        d(
            "DONE",
            "Completed",
            Flags {
                completed: true,
                ..talked
            },
        ),
    ]
}

/// The system codes and the codes of one campaign.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    campaign: Vec<Disposition>,
}

impl Catalog {
    /// A catalog with the codes of a campaign. A campaign code cannot have
    /// the code of a system disposition.
    pub fn new(campaign: Vec<Disposition>) -> Result<Self, CatalogError> {
        let system = system();
        let mut seen = std::collections::HashSet::new();
        for c in &campaign {
            let code_ok = (1..=16).contains(&c.code.len())
                && c.code
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
            if !code_ok {
                return Err(CatalogError::Code(c.code.clone()));
            }
            if system.iter().any(|s| s.code == c.code) {
                return Err(CatalogError::System(c.code.clone()));
            }
            if !seen.insert(c.code.as_str()) {
                return Err(CatalogError::Duplicate(c.code.clone()));
            }
            let name = c.name.trim();
            if name.is_empty() || name.chars().count() > 64 {
                return Err(CatalogError::Name(c.code.clone()));
            }
            let f = c.flags;
            let conflict = if f.dnc && f.scheduled_callback {
                Some("dnc and scheduled_callback exclude each other")
            } else if f.answering_machine && f.human_answered {
                Some("answering_machine and human_answered exclude each other")
            } else if f.answering_machine && (f.sale || f.dnc || f.scheduled_callback) {
                Some("a machine answer cannot be a sale, a DNC request, or a callback")
            } else {
                None
            };
            if let Some(why) = conflict {
                return Err(CatalogError::Flags {
                    code: c.code.clone(),
                    why,
                });
            }
        }
        Ok(Catalog { campaign })
    }

    /// The codes of the campaign, without the system codes.
    pub fn campaign(&self) -> &[Disposition] {
        &self.campaign
    }

    /// Every code: the system codes first.
    pub fn all(&self) -> Vec<Disposition> {
        let mut all = system();
        all.extend(self.campaign.iter().cloned());
        all
    }

    pub fn get(&self, code: &str) -> Option<Disposition> {
        self.all().into_iter().find(|d| d.code == code)
    }
}

//! Postal codes. `Rules` knows the forms of the United States and Canada, and
//! keeps the codes of other countries as they are, in upper case.

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PostalError {
    #[error("the postal code is empty")]
    Empty,
    #[error("'{code}' is not a postal code of {region}")]
    Wrong { code: String, region: String },
}

/// A set of rules that read postal codes.
pub trait PostalBackend: Send + Sync {
    /// The standard form of a postal code of `region` (ISO 3166). With no
    /// region, only the form of the text is checked.
    fn normalize(&self, raw: &str, region: Option<&str>) -> Result<String, PostalError>;
}

/// The rules of the dialer.
pub struct Rules;

/// The longest postal code that the rules keep.
const MAX_LEN: usize = 12;

impl PostalBackend for Rules {
    fn normalize(&self, raw: &str, region: Option<&str>) -> Result<String, PostalError> {
        let text = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() {
            return Err(PostalError::Empty);
        }
        let upper = text.to_ascii_uppercase();
        let wrong = |region: &str| PostalError::Wrong {
            code: text.clone(),
            region: region.to_string(),
        };
        match region {
            Some("US") => {
                let digits: String = upper.chars().filter(|c| *c != '-' && *c != ' ').collect();
                if !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(wrong("US"));
                }
                match digits.len() {
                    5 => Ok(digits),
                    9 => Ok(format!("{}-{}", &digits[..5], &digits[5..])),
                    _ => Err(wrong("US")),
                }
            }
            Some("CA") => {
                let c: Vec<char> = upper.chars().filter(|c| *c != ' ').collect();
                let form = c.len() == 6
                    && c.iter().enumerate().all(|(i, ch)| {
                        if i % 2 == 0 {
                            ch.is_ascii_alphabetic()
                        } else {
                            ch.is_ascii_digit()
                        }
                    });
                if form {
                    let s: String = c.into_iter().collect();
                    Ok(format!("{} {}", &s[..3], &s[3..]))
                } else {
                    Err(wrong("CA"))
                }
            }
            other => {
                let ok = upper.len() <= MAX_LEN
                    && upper
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '-');
                if ok {
                    Ok(upper)
                } else {
                    Err(wrong(other.unwrap_or("any country")))
                }
            }
        }
    }
}

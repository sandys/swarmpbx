//! Evaluation context: a string→string variable map, `${...}` interpolation,
//! and a registry of pure functions. This is the data a ruleset evaluates
//! against (lead fields, call vars, campaign settings, capture groups).
//!
//! The function surface re-expresses the parts of the FusionPBX dialplan API
//! we need — but pure and sandboxed. No Lua, no shell-out.

use std::collections::BTreeMap;

/// Variable context. Capture groups from condition regexes are stored as
/// `$1`, `$2`, ... and are also addressable in interpolation.
#[derive(Debug, Clone, Default)]
pub struct Context {
    vars: BTreeMap<String, String>,
}

impl Context {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut c = Context::new();
        for (k, v) in pairs {
            c.set(k, v);
        }
        c
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    pub fn set(&mut self, key: impl Into<String>, val: impl Into<String>) {
        self.vars.insert(key.into(), val.into());
    }

    pub fn unset(&mut self, key: &str) {
        self.vars.remove(key);
    }

    /// Store capture groups $1.. from a regex match.
    pub fn set_captures(&mut self, caps: &[Option<String>]) {
        for (i, c) in caps.iter().enumerate() {
            if let Some(val) = c {
                self.set(format!("${}", i + 1), val.clone());
            }
        }
    }

    /// Expand `${var}` and `$N` references and `fn(args)` function calls in a
    /// string. One pass, left to right. Unknown vars expand to empty.
    pub fn interpolate(&self, input: &str) -> String {
        interpolate(self, input)
    }
}

/// A pure function callable from rule data. Receives already-interpolated args.
type RoutingFn = fn(&Context, &[String]) -> String;

fn builtin(name: &str) -> Option<RoutingFn> {
    Some(match name {
        // cond(a == b ? x : y) is expressed as cond(a,b,x,y): x if a==b else y.
        "cond_eq" => |_, a| {
            if a.len() == 4 && a[0] == a[1] {
                a[2].clone()
            } else if a.len() == 4 {
                a[3].clone()
            } else {
                String::new()
            }
        },
        // regex_match(value, pattern) -> "true"/"false".
        "regex_match" => |_, a| {
            if a.len() == 2 {
                match regex::Regex::new(&a[1]) {
                    Ok(re) => (if re.is_match(&a[0]) { "true" } else { "false" }).to_string(),
                    Err(_) => "false".to_string(),
                }
            } else {
                "false".to_string()
            }
        },
        // upper/lower helpers.
        "upper" => |_, a| a.first().map(|s| s.to_uppercase()).unwrap_or_default(),
        "lower" => |_, a| a.first().map(|s| s.to_lowercase()).unwrap_or_default(),
        _ => return None,
    })
}

/// Interpolate `${...}` tokens. Supports `${var}`, `$N`, and `${fn:a,b,c}`.
fn interpolate(ctx: &Context, input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            // Find the matching closing brace (no nesting for this spike).
            if let Some(end) = input[i + 2..].find('}') {
                let token = &input[i + 2..i + 2 + end];
                out.push_str(&resolve_token(ctx, token));
                i = i + 2 + end + 1;
                continue;
            }
        }
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
            // $N capture reference.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            let key = &input[i..j];
            out.push_str(ctx.get(key).unwrap_or(""));
            i = j;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Resolve a `${...}` token: a function call `fn:a,b` or a plain variable.
fn resolve_token(ctx: &Context, token: &str) -> String {
    if let Some((name, args_str)) = token.split_once(':') {
        if let Some(f) = builtin(name) {
            let args: Vec<String> = args_str
                .split(',')
                .map(|a| ctx.interpolate(a.trim()))
                .collect();
            return f(ctx, &args);
        }
    }
    // Plain variable (interpolate nested refs defensively).
    ctx.get(token).map(|s| s.to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolate_vars_and_captures() {
        let mut c = Context::new();
        c.set("name", "jane");
        c.set_captures(&[Some("5551234".into())]);
        assert_eq!(c.interpolate("hi ${name} / $1"), "hi jane / 5551234");
        assert_eq!(c.interpolate("${missing}"), "");
    }

    #[test]
    fn function_cond_eq_and_regex() {
        let c = Context::from_pairs([("dir", "inbound")]);
        assert_eq!(c.interpolate("${cond_eq:inbound,inbound,yes,no}"), "yes");
        assert_eq!(c.interpolate("${cond_eq:a,b,yes,no}"), "no");
        assert_eq!(c.interpolate("${regex_match:+13125550100,^\\+1}"), "true");
        assert_eq!(c.interpolate("${upper:ab}"), "AB");
    }
}

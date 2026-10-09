//! Minimal bencode for the rtpengine ng protocol (spike).
//! Supports the subset rtpengine uses: byte-strings, integers, lists, dicts.
//! This build of rtpengine decodes the UDP ng dict as bencode (not JSON),
//! so we encode/decode bencode directly.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Ben {
    Str(String),
    Int(i64),
    List(Vec<Ben>),
    Dict(BTreeMap<String, Ben>),
}

impl Ben {
    pub fn as_str(&self) -> Option<&str> {
        if let Ben::Str(s) = self { Some(s) } else { None }
    }
    pub fn get(&self, key: &str) -> Option<&Ben> {
        if let Ben::Dict(m) = self { m.get(key) } else { None }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Ben::Str(s) => {
                out.extend_from_slice(s.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(s.as_bytes());
            }
            Ben::Int(i) => {
                out.push(b'i');
                out.extend_from_slice(i.to_string().as_bytes());
                out.push(b'e');
            }
            Ben::List(items) => {
                out.push(b'l');
                for it in items { it.encode_into(out); }
                out.push(b'e');
            }
            Ben::Dict(m) => {
                out.push(b'd');
                // bencode requires keys sorted; BTreeMap already is.
                for (k, v) in m {
                    Ben::Str(k.clone()).encode_into(out);
                    v.encode_into(out);
                }
                out.push(b'e');
            }
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Ben, String> {
        let mut pos = 0;
        let v = decode_at(bytes, &mut pos)?;
        Ok(v)
    }
}

fn decode_at(b: &[u8], pos: &mut usize) -> Result<Ben, String> {
    if *pos >= b.len() { return Err("unexpected end".into()); }
    match b[*pos] {
        b'i' => {
            *pos += 1;
            let start = *pos;
            while *pos < b.len() && b[*pos] != b'e' { *pos += 1; }
            let n: i64 = std::str::from_utf8(&b[start..*pos])
                .map_err(|e| e.to_string())?
                .parse()
                .map_err(|_| "bad int".to_string())?;
            *pos += 1; // skip 'e'
            Ok(Ben::Int(n))
        }
        b'l' => {
            *pos += 1;
            let mut items = Vec::new();
            while *pos < b.len() && b[*pos] != b'e' {
                items.push(decode_at(b, pos)?);
            }
            *pos += 1;
            Ok(Ben::List(items))
        }
        b'd' => {
            *pos += 1;
            let mut map = BTreeMap::new();
            while *pos < b.len() && b[*pos] != b'e' {
                let key = match decode_at(b, pos)? {
                    Ben::Str(s) => s,
                    _ => return Err("dict key not a string".into()),
                };
                let val = decode_at(b, pos)?;
                map.insert(key, val);
            }
            *pos += 1;
            Ok(Ben::Dict(map))
        }
        c if c.is_ascii_digit() => {
            let start = *pos;
            while *pos < b.len() && b[*pos] != b':' { *pos += 1; }
            let len: usize = std::str::from_utf8(&b[start..*pos])
                .map_err(|e| e.to_string())?
                .parse()
                .map_err(|_| "bad str len".to_string())?;
            *pos += 1; // skip ':'
            let s = &b[*pos..*pos + len];
            *pos += len;
            Ok(Ben::Str(String::from_utf8_lossy(s).into_owned()))
        }
        other => Err(format!("unexpected byte: {}", other as char)),
    }
}

/// Convenience: build a dict from (key, Ben) pairs.
pub fn dict(pairs: Vec<(&str, Ben)>) -> Ben {
    let mut m = BTreeMap::new();
    for (k, v) in pairs { m.insert(k.to_string(), v); }
    Ben::Dict(m)
}

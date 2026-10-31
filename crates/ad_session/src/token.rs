use rand::RngCore;
use sha2::{Digest, Sha256};

/// A new random session token: 32 bytes from the OS, as hex.
pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The digest that the dialer stores and compares.
pub fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

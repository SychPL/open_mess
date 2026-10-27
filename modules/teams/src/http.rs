//! What the protocol code hands to the host's `http.fetch` (wit/module.wit), and secrets.

use std::fmt;

/// One request for `http.fetch`. No `Debug` on purpose: headers carry tokens and the body carries
/// message text, and neither may reach a log (AGENTS.md).
#[derive(Clone)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// A token or other secret; `Debug` prints `***`.
pub struct SecretString(String);

impl SecretString {
    pub fn new(secret: String) -> Self {
        Self(secret)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

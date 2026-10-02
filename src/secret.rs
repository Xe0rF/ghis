//! A secret that is zeroized on drop and never rendered.
//!
//! Forge-agnostic on purpose.  The value may come from gh's keyring, from a
//! `credential_command` a profile points at, or from any other credential
//! source; none of that belongs to a particular forge adapter.

use std::fmt;
use zeroize::Zeroizing;

/// Token bytes held in memory for as short a time as the lookup allows.
///
/// `Debug` is hand-written so no formatting path can print the value.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretToken(Zeroizing<Vec<u8>>);

impl SecretToken {
    pub fn new(value: impl Into<Vec<u8>>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_slice()
    }

    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(self.0.as_slice()).ok()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn into_bytes(mut self) -> Zeroizing<Vec<u8>> {
        Zeroizing::new(std::mem::take(&mut *self.0))
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretToken([redacted])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_renders_the_value() {
        assert_eq!(
            format!("{:?}", SecretToken::new(b"secret-token".to_vec())),
            "SecretToken([redacted])"
        );
        assert_eq!(
            format!("{:#?}", SecretToken::new("secret-token")),
            "SecretToken([redacted])"
        );
    }
}

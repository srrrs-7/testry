//! Content fingerprints.
//!
//! A fingerprint is a BLAKE3 digest rendered as `blake3:<64 lowercase hex>`.
//! [`FingerprintBuilder`] frames every field with its length, so two different
//! field sequences can never hash the same byte stream.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FingerprintError {
    #[error("fingerprint must start with `blake3:`")]
    MissingPrefix,
    #[error("fingerprint must be 64 lowercase hexadecimal characters after the prefix")]
    InvalidHex,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    pub const PREFIX: &'static str = "blake3:";

    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(*blake3::hash(bytes).as_bytes())
    }

    /// Wraps a BLAKE3 digest that was computed elsewhere, for example while
    /// streaming a large file.
    pub fn from_digest(digest: [u8; 32]) -> Self {
        Self(digest)
    }

    pub fn parse(text: &str) -> Result<Self, FingerprintError> {
        let hex = text
            .strip_prefix(Self::PREFIX)
            .ok_or(FingerprintError::MissingPrefix)?;
        let is_canonical =
            hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if !is_canonical {
            return Err(FingerprintError::InvalidHex);
        }
        let hash = blake3::Hash::from_hex(hex).map_err(|_| FingerprintError::InvalidHex)?;
        Ok(Self(*hash.as_bytes()))
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_hex(&self) -> String {
        blake3::Hash::from_bytes(self.0).to_hex().to_string()
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", Self::PREFIX, self.to_hex())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

impl TryFrom<String> for Fingerprint {
    type Error = FingerprintError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<Fingerprint> for String {
    fn from(value: Fingerprint) -> Self {
        value.to_string()
    }
}

/// Incremental fingerprint over named, length-framed fields.
pub struct FingerprintBuilder {
    hasher: blake3::Hasher,
}

impl FingerprintBuilder {
    /// `domain` separates fingerprints of different kinds of data.
    pub fn new(domain: &str) -> Self {
        let mut builder = Self {
            hasher: blake3::Hasher::new(),
        };
        builder.frame(domain.as_bytes());
        builder
    }

    pub fn bytes(&mut self, name: &str, value: &[u8]) -> &mut Self {
        self.frame(name.as_bytes());
        self.frame(value);
        self
    }

    pub fn text(&mut self, name: &str, value: &str) -> &mut Self {
        self.bytes(name, value.as_bytes())
    }

    pub fn fingerprint(&mut self, name: &str, value: &Fingerprint) -> &mut Self {
        self.bytes(name, value.as_bytes())
    }

    pub fn finish(&self) -> Fingerprint {
        Fingerprint(*self.hasher.finalize().as_bytes())
    }

    fn frame(&mut self, bytes: &[u8]) {
        let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        self.hasher.update(&length.to_le_bytes());
        self.hasher.update(bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_and_parses_canonical_form() {
        let fingerprint = Fingerprint::of_bytes(b"hello");
        let text = fingerprint.to_string();
        assert!(text.starts_with("blake3:"));
        assert_eq!(text.len(), "blake3:".len() + 64);
        assert_eq!(Fingerprint::parse(&text).unwrap(), fingerprint);
    }

    #[test]
    fn rejects_non_canonical_text() {
        let hex = Fingerprint::of_bytes(b"x").to_hex();
        assert_eq!(
            Fingerprint::parse(&hex),
            Err(FingerprintError::MissingPrefix)
        );
        assert_eq!(
            Fingerprint::parse(&format!("blake3:{}", hex.to_uppercase())),
            Err(FingerprintError::InvalidHex)
        );
        assert_eq!(
            Fingerprint::parse("blake3:abc"),
            Err(FingerprintError::InvalidHex)
        );
        assert_eq!(
            Fingerprint::parse("sha256:abc"),
            Err(FingerprintError::MissingPrefix)
        );
    }

    #[test]
    fn framing_prevents_field_boundary_collisions() {
        let a = FingerprintBuilder::new("t")
            .text("k", "ab")
            .text("k", "c")
            .finish();
        let b = FingerprintBuilder::new("t")
            .text("k", "a")
            .text("k", "bc")
            .finish();
        assert_ne!(a, b);
    }

    #[test]
    fn domain_separates_fingerprints() {
        let a = FingerprintBuilder::new("plan").text("k", "v").finish();
        let b = FingerprintBuilder::new("snapshot").text("k", "v").finish();
        assert_ne!(a, b);
    }

    #[test]
    fn serde_round_trip() {
        let fingerprint = Fingerprint::of_bytes(b"data");
        let json = serde_json::to_string(&fingerprint).unwrap();
        assert_eq!(
            serde_json::from_str::<Fingerprint>(&json).unwrap(),
            fingerprint
        );
        assert!(serde_json::from_str::<Fingerprint>("\"blake3:zz\"").is_err());
    }
}

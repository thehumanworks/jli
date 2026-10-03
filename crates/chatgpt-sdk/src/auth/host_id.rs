//! The stable, opaque id of one install of the app (`ext_agent_host_id`).

use std::fmt;
use std::str::FromStr;

use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// The id of the host an app runs on: a laptop install or a self-hosted VM.
///
/// Make it once, before the host's first sign-in, keep it for the host's life, and send it with
/// every authorization request. It is an identifier, not a credential, and must not identify the
/// user. Accepted forms are `urn:uuid:…`, `urn:ietf:params:oauth:jwk-thumbprint:…` and
/// `did:key:…`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HostId(String);

const UUID_PREFIX: &str = "urn:uuid:";
const PREFIXES: [&str; 3] = [UUID_PREFIX, "urn:ietf:params:oauth:jwk-thumbprint:", "did:key:"];

impl HostId {
    /// A new `urn:uuid:` id from a random (version 4) UUID.
    pub fn generate() -> Result<HostId> {
        let mut bytes = [0u8; 16];
        SystemRandom::new().fill(&mut bytes).map_err(|_| Error::Protocol("the system has no secure randomness".into()))?;
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(HostId(format!("{UUID_PREFIX}{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])))
    }

    /// The id as sent.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for HostId {
    type Err = Error;

    fn from_str(text: &str) -> Result<HostId> {
        let text = text.trim();
        let Some(prefix) = PREFIXES.iter().find(|prefix| text.starts_with(**prefix)) else {
            return Err(Error::Protocol(format!(
                "{text:?} is not a host id: it must start with one of {}",
                PREFIXES.join(", ")
            )));
        };
        let rest = &text[prefix.len()..];
        let valid = if *prefix == UUID_PREFIX {
            rest.len() == 36
                && rest
                    .char_indices()
                    .all(|(at, c)| if [8, 13, 18, 23].contains(&at) { c == '-' } else { c.is_ascii_hexdigit() })
        } else {
            !rest.is_empty() && !rest.contains(char::is_whitespace)
        };
        if !valid {
            return Err(Error::Protocol(format!("{text:?} is not a well-formed host id")));
        }
        Ok(HostId(text.to_string()))
    }
}

impl TryFrom<String> for HostId {
    type Error = Error;

    fn try_from(text: String) -> Result<HostId> {
        text.parse()
    }
}

impl From<HostId> for String {
    fn from(id: HostId) -> String {
        id.0
    }
}

impl fmt::Display for HostId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_id_is_a_version_4_uuid_urn() {
        let id = HostId::generate().unwrap();
        let uuid = id.as_str().strip_prefix("urn:uuid:").unwrap();
        assert_eq!(uuid.len(), 36);
        assert_eq!(&uuid[14..15], "4", "version 4");
        assert!(matches!(&uuid[19..20], "8" | "9" | "a" | "b"), "RFC 9562 variant");
        assert_eq!(id.as_str().parse::<HostId>().unwrap(), id);
        assert_ne!(HostId::generate().unwrap(), id);
    }

    #[test]
    fn only_the_documented_forms_are_accepted() {
        for good in [
            "urn:uuid:6f1c2d9e-8d4b-4c7a-9e1f-2b3c4d5e6f70",
            "urn:ietf:params:oauth:jwk-thumbprint:sha-256:NzbLsXh8uDCcd-6MNwXF4W_7noWXFZAfHkxZsRGC9Xs",
            "did:key:z6MkhaXgBZDvotDkL5257faiztiGiC2QtKLGpbnnEGta2doK",
        ] {
            assert!(good.parse::<HostId>().is_ok(), "{good}");
        }
        for bad in ["", "user@example.com", "urn:uuid:not-a-uuid", "did:web:example.com", "did:key:"] {
            assert!(bad.parse::<HostId>().is_err(), "{bad}");
        }
        let json = serde_json::to_string(&HostId::generate().unwrap()).unwrap();
        assert!(serde_json::from_str::<HostId>(&json).is_ok());
        assert!(serde_json::from_str::<HostId>("\"nope\"").is_err());
    }
}

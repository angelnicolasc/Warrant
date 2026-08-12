//! DSSE envelopes.
//!
//! The signature covers a Pre-Authentication Encoding of the payload and its
//! type, not the payload alone. That length-prefixed framing is what stops a
//! signature over one payload type being replayed as a signature over
//! another, and it is why this is implemented to the specification rather
//! than as "sign the JSON".
//!
//! Existing supply-chain tooling reads this format unmodified, which is the
//! whole reason for using it: a Warrant receipt is not a new artefact type
//! anyone has to learn.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};

/// Payload type for in-toto statements.
pub const IN_TOTO_PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";

/// A signature over a payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// Identifies the key that produced it.
    pub keyid: String,
    /// Base64-encoded signature bytes.
    pub sig: String,
}

/// A DSSE envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// Base64-encoded payload.
    pub payload: String,
    /// What the payload is.
    #[serde(rename = "payloadType")]
    pub payload_type: String,
    /// Signatures over the pre-authentication encoding.
    pub signatures: Vec<Signature>,
}

impl Envelope {
    /// The decoded payload bytes.
    pub fn decode_payload(&self) -> Option<Vec<u8>> {
        BASE64.decode(&self.payload).ok()
    }

    /// The bytes a signature must be verified against.
    pub fn signing_bytes(&self) -> Option<Vec<u8>> {
        Some(pae(&self.payload_type, &self.decode_payload()?))
    }
}

/// Pre-Authentication Encoding, per the DSSE specification.
///
/// ```text
/// PAE(type, body) = "DSSEv1" SP LEN(type) SP type SP LEN(body) SP body
/// ```
///
/// where `LEN` is the byte length in ASCII decimal.
pub fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + payload_type.len() + 32);
    out.extend_from_slice(b"DSSEv1 ");
    out.extend_from_slice(payload_type.len().to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload_type.as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload.len().to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload);
    out
}

/// Base64-encode, for building envelopes.
pub fn encode(bytes: &[u8]) -> String {
    BASE64.encode(bytes)
}

/// Base64-decode.
pub fn decode(text: &str) -> Option<Vec<u8>> {
    BASE64.decode(text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example from the DSSE specification.
    #[test]
    fn pae_matches_the_specification_example() {
        assert_eq!(pae("http/vnd.test", b"test"), b"DSSEv1 13 http/vnd.test 4 test".to_vec());
    }

    #[test]
    fn pae_is_unambiguous_across_the_type_body_boundary() {
        // Without length prefixes these two would frame identically.
        assert_ne!(pae("ab", b"cd"), pae("abc", b"d"));
        assert_ne!(pae("a", b"bcd"), pae("ab", b"cd"));
    }

    #[test]
    fn pae_handles_empty_and_binary_payloads() {
        assert_eq!(pae("t", b""), b"DSSEv1 1 t 0 ".to_vec());
        let binary = [0u8, 255, 10, 13];
        let framed = pae("t", &binary);
        assert!(framed.ends_with(&binary));
    }

    #[test]
    fn an_envelope_round_trips_through_json() {
        let envelope = Envelope {
            payload: encode(br#"{"_type":"x"}"#),
            payload_type: IN_TOTO_PAYLOAD_TYPE.into(),
            signatures: vec![Signature { keyid: "abc".into(), sig: encode(&[1, 2, 3]) }],
        };
        let json = serde_json::to_string(&envelope).unwrap();
        assert!(json.contains("payloadType"), "the field name is fixed by the specification");
        assert_eq!(serde_json::from_str::<Envelope>(&json).unwrap(), envelope);
        assert_eq!(envelope.decode_payload().unwrap(), br#"{"_type":"x"}"#);
    }
}

//! CredSSP `TSRequest` and `TSCredentials` DER (de)serialization (MS-CSSP 2.2.1).
//!
//! CredSSP wraps the SPNEGO/NTLM/Kerberos token exchange (the `negoTokens`),
//! then carries the public-key authentication blob (`pubKeyAuth`) and finally
//! the encrypted credentials (`authInfo`). The ASN.1 module uses EXPLICIT
//! tagging, so each `[n]` field is a constructed context tag wrapping the base
//! type.
//!
//! ```text
//! TSRequest ::= SEQUENCE {
//!     version    [0] INTEGER,
//!     negoTokens [1] NegoData OPTIONAL,
//!     authInfo   [2] OCTET STRING OPTIONAL,
//!     pubKeyAuth [3] OCTET STRING OPTIONAL,
//!     errorCode  [4] INTEGER OPTIONAL,        -- v3+
//!     clientNonce[5] OCTET STRING OPTIONAL    -- v5+
//! }
//! NegoData ::= SEQUENCE OF SEQUENCE { negoToken [0] OCTET STRING }
//! ```

use rdp_asn1::der;
use rdp_asn1::Asn1Error;

/// CredSSP protocol version we advertise. v6 is what current Windows speaks.
pub const CREDSSP_VERSION: u32 = 6;

/// A CredSSP `TSRequest`, whether parsed from the wire or built to send.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TsRequest {
    pub version: u32,
    /// SPNEGO/NTLM/Kerberos tokens (usually exactly one per round trip).
    pub nego_tokens: Vec<Vec<u8>>,
    /// Encrypted `TSCredentials` (sent last).
    pub auth_info: Option<Vec<u8>>,
    /// Public-key authentication blob (anti-MITM binding to the TLS channel).
    pub pub_key_auth: Option<Vec<u8>>,
    /// Error code from the server (v3+).
    pub error_code: Option<u32>,
    /// Client nonce used in the v5+ public-key hash.
    pub client_nonce: Option<[u8; 32]>,
}

impl TsRequest {
    /// A request carrying a single negotiation token.
    pub fn with_nego_token(token: Vec<u8>) -> Self {
        Self {
            version: CREDSSP_VERSION,
            nego_tokens: vec![token],
            ..Default::default()
        }
    }

    /// Serialize to DER.
    pub fn to_der(&self) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend(der::context(0, &der::integer(self.version)));

        if !self.nego_tokens.is_empty() {
            let mut items = Vec::new();
            for token in &self.nego_tokens {
                // SEQUENCE { negoToken [0] OCTET STRING }
                items.extend(der::sequence(&der::context(0, &der::octet_string(token))));
            }
            body.extend(der::context(1, &der::sequence(&items)));
        }
        if let Some(auth) = &self.auth_info {
            body.extend(der::context(2, &der::octet_string(auth)));
        }
        if let Some(pk) = &self.pub_key_auth {
            body.extend(der::context(3, &der::octet_string(pk)));
        }
        if let Some(code) = self.error_code {
            body.extend(der::context(4, &der::integer(code)));
        }
        if let Some(nonce) = &self.client_nonce {
            body.extend(der::context(5, &der::octet_string(nonce)));
        }
        der::sequence(&body)
    }

    /// Parse from DER.
    pub fn from_der(input: &[u8]) -> Result<Self, Asn1Error> {
        let mut cur = input;
        let body = der::expect(&mut cur, der::TAG_SEQUENCE)?;
        let mut fields = body;
        let mut req = TsRequest::default();

        while !fields.is_empty() {
            let (tag, value) = der::read_element(&mut fields)?;
            let mut inner = value;
            match tag {
                t if t == der::context_tag(0) => {
                    req.version = der::read_u32(der::expect(&mut inner, der::TAG_INTEGER)?)?;
                }
                t if t == der::context_tag(1) => {
                    let seq = der::expect(&mut inner, der::TAG_SEQUENCE)?;
                    let mut items = seq;
                    while !items.is_empty() {
                        let mut one = der::expect(&mut items, der::TAG_SEQUENCE)?;
                        let mut token = der::expect(&mut one, der::context_tag(0))?;
                        let octets = der::expect(&mut token, der::TAG_OCTET_STRING)?;
                        req.nego_tokens.push(octets.to_vec());
                    }
                }
                t if t == der::context_tag(2) => {
                    req.auth_info = Some(der::expect(&mut inner, der::TAG_OCTET_STRING)?.to_vec());
                }
                t if t == der::context_tag(3) => {
                    req.pub_key_auth =
                        Some(der::expect(&mut inner, der::TAG_OCTET_STRING)?.to_vec());
                }
                t if t == der::context_tag(4) => {
                    req.error_code =
                        Some(der::read_u32(der::expect(&mut inner, der::TAG_INTEGER)?)?);
                }
                t if t == der::context_tag(5) => {
                    let octets = der::expect(&mut inner, der::TAG_OCTET_STRING)?;
                    if octets.len() == 32 {
                        let mut nonce = [0u8; 32];
                        nonce.copy_from_slice(octets);
                        req.client_nonce = Some(nonce);
                    }
                }
                _ => { /* unknown/optional field — ignore for forward-compat */ }
            }
        }
        Ok(req)
    }
}

/// Build the DER of a `TSCredentials` carrying `TSPasswordCreds` (credType 1).
///
/// The domain/user/password are encoded as UTF-16LE, per RDP convention. The
/// returned bytes are the plaintext that the caller encrypts (via SSPI
/// `EncryptMessage`) and places in `TSRequest.authInfo`.
pub fn password_credentials_der(domain: &str, username: &str, password: &str) -> Vec<u8> {
    let domain = utf16le(domain);
    let username = utf16le(username);
    let password = utf16le(password);

    let ts_password = der::sequence(
        &[
            der::context(0, &der::octet_string(&domain)),
            der::context(1, &der::octet_string(&username)),
            der::context(2, &der::octet_string(&password)),
        ]
        .concat(),
    );

    der::sequence(
        &[
            der::context(0, &der::integer(1)), // credType = TSPasswordCreds
            der::context(1, &der::octet_string(&ts_password)),
        ]
        .concat(),
    )
}

fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_only_request_matches_known_bytes() {
        let req = TsRequest {
            version: 6,
            ..Default::default()
        };
        // SEQUENCE { [0] { INTEGER 6 } } = 30 05 a0 03 02 01 06
        assert_eq!(req.to_der(), vec![0x30, 0x05, 0xa0, 0x03, 0x02, 0x01, 0x06]);
    }

    #[test]
    fn nego_token_roundtrips() {
        let req = TsRequest::with_nego_token(b"NTLMSSP\0".to_vec());
        let der_bytes = req.to_der();
        let parsed = TsRequest::from_der(&der_bytes).unwrap();
        assert_eq!(parsed, req);
        assert_eq!(parsed.nego_tokens[0], b"NTLMSSP\0");
        assert_eq!(parsed.version, 6);
    }

    #[test]
    fn all_optional_fields_roundtrip() {
        let req = TsRequest {
            version: 6,
            nego_tokens: vec![vec![1, 2, 3], vec![4, 5]],
            auth_info: Some(vec![0xaa; 40]),
            pub_key_auth: Some(vec![0xbb; 16]),
            error_code: Some(0xc000_0022),
            client_nonce: Some([7u8; 32]),
        };
        let parsed = TsRequest::from_der(&req.to_der()).unwrap();
        assert_eq!(parsed, req);
    }

    #[test]
    fn server_response_with_error_code_parses() {
        // Minimal server TSRequest: version 6 + errorCode 0x0000000c.
        let server = TsRequest {
            version: 6,
            error_code: Some(0x0000_000c),
            ..Default::default()
        };
        let parsed = TsRequest::from_der(&server.to_der()).unwrap();
        assert_eq!(parsed.error_code, Some(0x0000_000c));
        assert!(parsed.nego_tokens.is_empty());
    }

    #[test]
    fn password_credentials_structure() {
        let creds = password_credentials_der("CORP", "alice", "hunter2");
        let mut cur: &[u8] = &creds;
        let body = der::expect(&mut cur, der::TAG_SEQUENCE).unwrap();
        let mut b = body;
        // credType [0] INTEGER == 1
        let mut cred_type = der::expect(&mut b, der::context_tag(0)).unwrap();
        assert_eq!(
            der::read_u32(der::expect(&mut cred_type, der::TAG_INTEGER).unwrap()).unwrap(),
            1
        );
        // credentials [1] OCTET STRING wrapping a TSPasswordCreds SEQUENCE
        let mut creds_field = der::expect(&mut b, der::context_tag(1)).unwrap();
        let inner = der::expect(&mut creds_field, der::TAG_OCTET_STRING).unwrap();
        let mut ic: &[u8] = inner;
        let pwd_body = der::expect(&mut ic, der::TAG_SEQUENCE).unwrap();
        // domain [0], user [1], password [2] all present
        let mut pb = pwd_body;
        let domain = der::expect(&mut pb, der::context_tag(0)).unwrap();
        let user = der::expect(&mut pb, der::context_tag(1)).unwrap();
        let pass = der::expect(&mut pb, der::context_tag(2)).unwrap();
        assert!(!domain.is_empty() && !user.is_empty() && !pass.is_empty());
    }
}

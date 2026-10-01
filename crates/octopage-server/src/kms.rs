use std::sync::Arc;

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hmac::{Hmac, Mac};
use octopage::KeyProvider;
use sha2::{Digest, Sha256};

/// A master key held by the service: wraps with XChaCha20-Poly1305.
pub struct LocalKms {
    id: String,
    key: [u8; 32],
}

impl LocalKms {
    /// `name` identifies the master key (stored beside each key it wraps).
    pub fn new(name: &str, key: [u8; 32]) -> Arc<Self> {
        Arc::new(LocalKms {
            id: format!("local:{name}"),
            key,
        })
    }

    /// From 64 hex digits.
    pub fn from_hex(name: &str, hex: &str) -> Result<Arc<Self>, String> {
        let bytes = decode_hex(hex.trim()).ok_or("the master key is 64 hex digits")?;
        let key: [u8; 32] = bytes
            .try_into()
            .map_err(|_| "the master key is 64 hex digits")?;
        Ok(LocalKms::new(name, key))
    }
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl KeyProvider for LocalKms {
    fn id(&self) -> String {
        self.id.clone()
    }

    fn wrap(&self, key: &[u8; 32]) -> Result<Vec<u8>, String> {
        let mut nonce = [0u8; 24];
        getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
        let cipher = XChaCha20Poly1305::new_from_slice(&self.key).map_err(|e| e.to_string())?;
        let sealed = cipher
            .encrypt(&XNonce::from(nonce), &key[..])
            .map_err(|_| "wrapping failed")?;
        Ok([&nonce[..], &sealed].concat())
    }

    fn unwrap(&self, wrapped: &[u8]) -> Result<[u8; 32], String> {
        let (nonce, sealed) = wrapped
            .split_at_checked(24)
            .ok_or("a wrapped key too short")?;
        let cipher = XChaCha20Poly1305::new_from_slice(&self.key).map_err(|e| e.to_string())?;
        let nonce: [u8; 24] = nonce.try_into().expect("split at 24");
        let plain = cipher
            .decrypt(&XNonce::from(nonce), sealed)
            .map_err(|_| "this master key did not wrap that key")?;
        plain
            .try_into()
            .map_err(|_| "a wrapped key of the wrong size".into())
    }
}

/// AWS credentials.
#[derive(Clone)]
pub struct AwsCredentials {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
}

impl std::fmt::Debug for AwsCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AwsCredentials({}, ***)", self.access_key)
    }
}

/// AWS KMS: `Encrypt` and `Decrypt` on one key.
pub struct AwsKms {
    region: String,
    key_id: String,
    credentials: AwsCredentials,
    endpoint: String,
    http: reqwest::Client,
    runtime: tokio::runtime::Handle,
}

impl AwsKms {
    /// Key `key_id` (an ARN or alias) in `region`. Call it on the service's runtime.
    pub fn new(region: &str, key_id: &str, credentials: AwsCredentials) -> Arc<Self> {
        Arc::new(AwsKms {
            region: region.to_string(),
            key_id: key_id.to_string(),
            endpoint: format!("https://kms.{region}.amazonaws.com/"),
            credentials,
            http: reqwest::Client::new(),
            runtime: tokio::runtime::Handle::current(),
        })
    }

    fn call(&self, target: &str, body: serde_json::Value) -> Result<serde_json::Value, String> {
        let body = body.to_string();
        let now = std::time::SystemTime::now();
        let host = self
            .endpoint
            .trim_start_matches("https://")
            .trim_end_matches('/')
            .to_string();
        let mut headers = vec![
            (
                "content-type".to_string(),
                "application/x-amz-json-1.1".to_string(),
            ),
            ("host".to_string(), host),
            ("x-amz-target".to_string(), format!("TrentService.{target}")),
        ];
        if let Some(token) = &self.credentials.session_token {
            headers.push(("x-amz-security-token".into(), token.clone()));
        }
        let signed = sign_v4(
            &SigningRequest {
                method: "POST",
                path: "/",
                query: "",
                headers: &headers,
                body: body.as_bytes(),
            },
            &self.credentials,
            &self.region,
            "kms",
            now,
        );
        let mut request = self.http.post(&self.endpoint).body(body);
        for (name, value) in signed {
            request = request.header(name, value);
        }
        // Key providers are called synchronously while a database opens.
        tokio::task::block_in_place(|| {
            self.runtime.block_on(async {
                let response = request.send().await.map_err(|e| format!("AWS KMS: {e}"))?;
                let status = response.status();
                let text = response.text().await.map_err(|e| e.to_string())?;
                if !status.is_success() {
                    return Err(format!("AWS KMS {target} ({status}): {text}"));
                }
                serde_json::from_str(&text).map_err(|e| e.to_string())
            })
        })
    }
}

impl KeyProvider for AwsKms {
    fn id(&self) -> String {
        format!("aws-kms:{}", self.key_id)
    }

    fn wrap(&self, key: &[u8; 32]) -> Result<Vec<u8>, String> {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        let answer = self.call(
            "Encrypt",
            serde_json::json!({ "KeyId": self.key_id, "Plaintext": b64.encode(key) }),
        )?;
        let blob = answer["CiphertextBlob"]
            .as_str()
            .ok_or("AWS KMS returned no ciphertext")?;
        b64.decode(blob).map_err(|e| e.to_string())
    }

    fn unwrap(&self, wrapped: &[u8]) -> Result<[u8; 32], String> {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        let answer = self.call(
            "Decrypt",
            serde_json::json!({ "KeyId": self.key_id, "CiphertextBlob": b64.encode(wrapped) }),
        )?;
        let plain = b64
            .decode(
                answer["Plaintext"]
                    .as_str()
                    .ok_or("AWS KMS returned no key")?,
            )
            .map_err(|e| e.to_string())?;
        plain
            .try_into()
            .map_err(|_| "AWS KMS returned a key of the wrong size".into())
    }
}

/// A request to sign.
pub struct SigningRequest<'a> {
    pub method: &'a str,
    pub path: &'a str,
    /// Already in canonical form (sorted, encoded).
    pub query: &'a str,
    /// Lower-case names.
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(key).expect("any key size");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// AWS Signature Version 4: the headers to send (the request's own, plus `x-amz-date` and
/// `authorization`).
pub fn sign_v4(
    request: &SigningRequest<'_>,
    credentials: &AwsCredentials,
    region: &str,
    service: &str,
    at: std::time::SystemTime,
) -> Vec<(String, String)> {
    let seconds = at
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (date, stamp) = amz_date(seconds);
    let mut headers: Vec<(String, String)> = request.headers.to_vec();
    headers.push(("x-amz-date".into(), stamp.clone()));
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    let canonical_headers: String = headers
        .iter()
        .map(|(n, v)| format!("{n}:{}\n", v.trim()))
        .collect();
    let signed_headers = headers
        .iter()
        .map(|(n, _)| n.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{}",
        request.method,
        request.path,
        request.query,
        hex(&Sha256::digest(request.body))
    );
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}",
        hex(&Sha256::digest(canonical.as_bytes()))
    );
    let k_date = hmac(format!("AWS4{}", credentials.secret_key).as_bytes(), &date);
    let k_region = hmac(&k_date, region);
    let k_service = hmac(&k_region, service);
    let k_signing = hmac(&k_service, "aws4_request");
    let signature = hex(&hmac(&k_signing, &to_sign));
    headers.push((
        "authorization".into(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            credentials.access_key
        ),
    ));
    headers
}

/// (`YYYYMMDD`, `YYYYMMDDTHHMMSSZ`) for seconds since the Unix epoch.
fn amz_date(seconds: i64) -> (String, String) {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    // Civil date from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    let date = format!("{year:04}{month:02}{day:02}");
    let stamp = format!(
        "{date}T{:02}{:02}{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    );
    (date, stamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_master_key_wraps_and_unwraps() {
        let kms = LocalKms::new("test", [7; 32]);
        let key = [42u8; 32];
        let wrapped = kms.wrap(&key).unwrap();
        assert_ne!(&wrapped[24..56], &key[..]);
        assert_eq!(kms.unwrap(&wrapped).unwrap(), key);
        let other = LocalKms::new("other", [8; 32]);
        assert!(other.unwrap(&wrapped).is_err());
        assert!(LocalKms::from_hex("x", &"ab".repeat(32)).is_ok());
        assert!(LocalKms::from_hex("x", "abc").is_err());
    }

    /// AWS's documented Signature Version 4 example (IAM ListUsers).
    #[test]
    fn signature_v4_matches_aws_example() {
        let credentials = AwsCredentials {
            access_key: "AKIDEXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        let headers = vec![
            (
                "content-type".to_string(),
                "application/x-www-form-urlencoded; charset=utf-8".to_string(),
            ),
            ("host".to_string(), "iam.amazonaws.com".to_string()),
        ];
        let at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_440_938_160);
        let signed = sign_v4(
            &SigningRequest {
                method: "GET",
                path: "/",
                query: "Action=ListUsers&Version=2010-05-08",
                headers: &headers,
                body: b"",
            },
            &credentials,
            "us-east-1",
            "iam",
            at,
        );
        let auth = &signed.iter().find(|(n, _)| n == "authorization").unwrap().1;
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, \
             SignedHeaders=content-type;host;x-amz-date, \
             Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }
}

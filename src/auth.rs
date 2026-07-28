use axum::http::request::Parts;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone)]
pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
}

#[derive(Debug)]
struct ParsedAuthorization<'a> {
    access_key: &'a str,
    date: &'a str,
    region: &'a str,
    service: &'a str,
    signed_headers: &'a str,
    signature: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    MissingHeader,
    InvalidAuthorization,
    InvalidAccessKey,
    SignatureMismatch,
    PayloadHashMismatch,
}

#[derive(Debug, Clone)]
pub struct SignedRequestHeaders {
    pub authorization: String,
    pub amz_date: String,
    pub payload_hash: String,
}

pub fn sign_request(
    method: &str,
    canonical_path: &str,
    query: &str,
    host: &str,
    body: &[u8],
    credentials: &Credentials,
    region: &str,
) -> SignedRequestHeaders {
    sign_request_at(
        method,
        canonical_path,
        query,
        host,
        body,
        credentials,
        region,
        Utc::now(),
    )
}

#[allow(clippy::too_many_arguments)]
fn sign_request_at(
    method: &str,
    canonical_path: &str,
    query: &str,
    host: &str,
    body: &[u8],
    credentials: &Credentials,
    region: &str,
    timestamp: DateTime<Utc>,
) -> SignedRequestHeaders {
    let amz_date = timestamp.format("%Y%m%dT%H%M%SZ").to_string();
    let date = timestamp.format("%Y%m%d").to_string();
    let payload_hash = hex::encode(Sha256::digest(body));
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_headers = format!(
        "host:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
        normalize_header_value(host),
        payload_hash,
        amz_date
    );
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method,
        canonical_path,
        canonical_query(query),
        canonical_headers,
        signed_headers,
        payload_hash
    );
    let scope = format!("{date}/{region}/s3/aws4_request");
    let canonical_hash = hex::encode(Sha256::digest(canonical_request.as_bytes()));
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{canonical_hash}");
    let signature = calculate_signature(
        &credentials.secret_key,
        &date,
        region,
        "s3",
        string_to_sign.as_bytes(),
    );

    SignedRequestHeaders {
        authorization: format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            credentials.access_key
        ),
        amz_date,
        payload_hash,
    }
}

pub fn verify(parts: &Parts, credentials: &Credentials) -> Result<(), AuthError> {
    let authorization = header(parts, "authorization").ok_or(AuthError::MissingHeader)?;
    let amz_date = header(parts, "x-amz-date").ok_or(AuthError::MissingHeader)?;
    let payload_hash = header(parts, "x-amz-content-sha256").unwrap_or("UNSIGNED-PAYLOAD");
    let parsed = parse_authorization(authorization)?;

    if parsed.access_key != credentials.access_key {
        return Err(AuthError::InvalidAccessKey);
    }
    if parsed.service != "s3"
        || !amz_date.starts_with(parsed.date)
        || parsed.signature.len() != 64
        || !parsed
            .signature
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(AuthError::InvalidAuthorization);
    }

    let canonical_request = canonical_request(parts, parsed.signed_headers, payload_hash)?;
    let canonical_hash = hex::encode(Sha256::digest(canonical_request.as_bytes()));
    let scope = format!(
        "{}/{}/{}/aws4_request",
        parsed.date, parsed.region, parsed.service
    );
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{canonical_hash}");
    let expected = calculate_signature(
        &credentials.secret_key,
        parsed.date,
        parsed.region,
        parsed.service,
        string_to_sign.as_bytes(),
    );

    if expected
        .as_bytes()
        .ct_eq(parsed.signature.to_ascii_lowercase().as_bytes())
        .into()
    {
        Ok(())
    } else {
        Err(AuthError::SignatureMismatch)
    }
}

pub fn verify_payload(parts: &Parts, body: &[u8]) -> Result<(), AuthError> {
    let Some(payload_hash) = header(parts, "x-amz-content-sha256") else {
        return Ok(());
    };

    if payload_hash == "UNSIGNED-PAYLOAD" || payload_hash.starts_with("STREAMING-") {
        return Ok(());
    }
    if payload_hash.len() != 64 {
        return Err(AuthError::PayloadHashMismatch);
    }

    let actual = hex::encode(Sha256::digest(body));
    if actual
        .as_bytes()
        .ct_eq(payload_hash.to_ascii_lowercase().as_bytes())
        .into()
    {
        Ok(())
    } else {
        Err(AuthError::PayloadHashMismatch)
    }
}

fn parse_authorization(header: &str) -> Result<ParsedAuthorization<'_>, AuthError> {
    let fields = header
        .strip_prefix("AWS4-HMAC-SHA256 ")
        .ok_or(AuthError::InvalidAuthorization)?;

    let mut credential = None;
    let mut signed_headers = None;
    let mut signature = None;
    for field in fields.split(',') {
        let (name, value) = field
            .trim()
            .split_once('=')
            .ok_or(AuthError::InvalidAuthorization)?;
        match name {
            "Credential" => credential = Some(value),
            "SignedHeaders" => signed_headers = Some(value),
            "Signature" => signature = Some(value),
            _ => {}
        }
    }

    let mut scope = credential
        .ok_or(AuthError::InvalidAuthorization)?
        .split('/');
    let parsed = ParsedAuthorization {
        access_key: scope.next().ok_or(AuthError::InvalidAuthorization)?,
        date: scope.next().ok_or(AuthError::InvalidAuthorization)?,
        region: scope.next().ok_or(AuthError::InvalidAuthorization)?,
        service: scope.next().ok_or(AuthError::InvalidAuthorization)?,
        signed_headers: signed_headers.ok_or(AuthError::InvalidAuthorization)?,
        signature: signature.ok_or(AuthError::InvalidAuthorization)?,
    };
    if scope.next() != Some("aws4_request") || scope.next().is_some() {
        return Err(AuthError::InvalidAuthorization);
    }
    Ok(parsed)
}

fn canonical_request(
    parts: &Parts,
    signed_headers: &str,
    payload_hash: &str,
) -> Result<String, AuthError> {
    let mut canonical_headers = String::new();
    for name in signed_headers.split(';') {
        if name.is_empty() || name.bytes().any(|byte| byte.is_ascii_uppercase()) {
            return Err(AuthError::InvalidAuthorization);
        }
        let value = header(parts, name).ok_or(AuthError::InvalidAuthorization)?;
        canonical_headers.push_str(name);
        canonical_headers.push(':');
        canonical_headers.push_str(&normalize_header_value(value));
        canonical_headers.push('\n');
    }

    Ok(format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        parts.method.as_str(),
        parts.uri.path(),
        canonical_query(parts.uri.query().unwrap_or_default()),
        canonical_headers,
        signed_headers,
        payload_hash
    ))
}

fn canonical_query(query: &str) -> String {
    let mut pairs = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            if pair.contains('=') {
                pair.to_owned()
            } else {
                format!("{pair}=")
            }
        })
        .collect::<Vec<_>>();
    pairs.sort_unstable();
    pairs.join("&")
}

fn normalize_header_value(value: &str) -> String {
    value.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

fn header<'a>(parts: &'a Parts, name: &str) -> Option<&'a str> {
    parts.headers.get(name)?.to_str().ok()
}

fn calculate_signature(
    secret_key: &str,
    date: &str,
    region: &str,
    service: &str,
    string_to_sign: &[u8],
) -> String {
    let date_key = hmac(format!("AWS4{secret_key}").as_bytes(), date.as_bytes());
    let region_key = hmac(&date_key, region.as_bytes());
    let service_key = hmac(&region_key, service.as_bytes());
    let signing_key = hmac(&service_key, b"aws4_request");
    hex::encode(hmac(&signing_key, string_to_sign))
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use chrono::{TimeZone, Utc};

    use super::{
        Credentials, canonical_query, normalize_header_value, sign_request_at, verify,
        verify_payload,
    };

    #[test]
    fn canonical_query_sorts_and_normalizes_bare_parameters() {
        assert_eq!(
            canonical_query("prefix=b&delete&list-type=2"),
            "delete=&list-type=2&prefix=b"
        );
    }

    #[test]
    fn canonical_header_values_collapse_whitespace() {
        assert_eq!(normalize_header_value("  a \t b   c  "), "a b c");
    }

    #[test]
    fn generated_signature_passes_verification() {
        let credentials = Credentials {
            access_key: "access".to_owned(),
            secret_key: "secret".to_owned(),
        };
        let body = b"hello";
        let signed = sign_request_at(
            "PUT",
            "/bucket/key",
            "partNumber=1&uploadId=test",
            "localhost:9000",
            body,
            &credentials,
            "us-east-1",
            Utc.with_ymd_and_hms(2026, 7, 27, 12, 0, 0).unwrap(),
        );
        let request = Request::builder()
            .method("PUT")
            .uri("/bucket/key?partNumber=1&uploadId=test")
            .header("host", "localhost:9000")
            .header("x-amz-date", signed.amz_date)
            .header("x-amz-content-sha256", signed.payload_hash)
            .header("authorization", signed.authorization)
            .body(Body::empty())
            .unwrap();
        let (parts, _) = request.into_parts();

        assert_eq!(verify(&parts, &credentials), Ok(()));
        assert_eq!(verify_payload(&parts, body), Ok(()));
    }
}

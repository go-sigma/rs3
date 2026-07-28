use std::fmt;

use anyhow::{Context, Result, anyhow, bail};
use quick_xml::Reader;
use quick_xml::events::Event;
use reqwest::header::{
    AUTHORIZATION, CONTENT_LENGTH, ETAG, HOST, HeaderMap, HeaderValue, LAST_MODIFIED,
};
use reqwest::{Method, Response, StatusCode, Url};

use crate::auth::{Credentials, sign_request};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Path {
    pub bucket: String,
    pub key: String,
}

impl S3Path {
    pub fn parse(input: &str) -> Result<Self> {
        let path = input
            .strip_prefix("s3://")
            .ok_or_else(|| anyhow!("remote path must start with s3://"))?;
        let (bucket, key) = path.split_once('/').unwrap_or((path, ""));
        if bucket.is_empty() {
            bail!("remote path must include a bucket");
        }
        Ok(Self {
            bucket: bucket.to_owned(),
            key: key.to_owned(),
        })
    }

    pub fn bucket(input: &str) -> Result<String> {
        let path = if input.starts_with("s3://") {
            Self::parse(input)?
        } else {
            Self {
                bucket: input.to_owned(),
                key: String::new(),
            }
        };
        if !path.key.is_empty() {
            bail!("bucket command does not accept an object key");
        }
        Ok(path.bucket)
    }
}

impl fmt::Display for S3Path {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.key.is_empty() {
            write!(formatter, "s3://{}", self.bucket)
        } else {
            write!(formatter, "s3://{}/{}", self.bucket, self.key)
        }
    }
}

#[derive(Debug, Clone)]
pub struct S3Client {
    http: reqwest::Client,
    endpoint: String,
    host: String,
    credentials: Credentials,
    region: String,
}

#[derive(Debug, Clone)]
pub struct BucketEntry {
    pub name: String,
    pub creation_date: String,
}

#[derive(Debug, Clone)]
pub enum ListEntry {
    Object {
        key: String,
        size: u64,
        modified: String,
    },
    Prefix(String),
}

#[derive(Debug, Clone)]
pub struct ObjectMetadata {
    pub size: u64,
    pub etag: Option<String>,
    pub modified: Option<String>,
}

impl S3Client {
    pub fn new(
        endpoint: &str,
        access_key: String,
        secret_key: String,
        region: String,
    ) -> Result<Self> {
        let parsed = Url::parse(endpoint).context("invalid endpoint URL")?;
        if !matches!(parsed.scheme(), "http" | "https") {
            bail!("endpoint scheme must be http or https");
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            bail!("endpoint must not contain a query string or fragment");
        }
        if parsed.path() != "/" && !parsed.path().is_empty() {
            bail!("endpoint must not contain a path");
        }

        let host = match parsed.host() {
            Some(url::Host::Domain(domain)) => domain.to_owned(),
            Some(url::Host::Ipv4(address)) => address.to_string(),
            Some(url::Host::Ipv6(address)) => format!("[{address}]"),
            None => bail!("endpoint must include a host"),
        };
        let host = if let Some(port) = parsed.port() {
            format!("{host}:{port}")
        } else {
            host
        };
        let endpoint = format!("{}://{}", parsed.scheme(), host);

        Ok(Self {
            http: reqwest::Client::builder().build()?,
            endpoint,
            host,
            credentials: Credentials {
                access_key,
                secret_key,
            },
            region,
        })
    }

    pub async fn make_bucket(&self, bucket: &str) -> Result<()> {
        self.send(Method::PUT, &bucket_path(bucket), "", Vec::new())
            .await?;
        Ok(())
    }

    pub async fn bucket_exists(&self, bucket: &str) -> Result<bool> {
        let response = self
            .request(Method::HEAD, &bucket_path(bucket), "", Vec::new())
            .await?;
        match response.status() {
            status if status.is_success() => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            _ => {
                ensure_success(response).await?;
                Ok(true)
            }
        }
    }

    pub async fn remove_bucket(&self, bucket: &str) -> Result<()> {
        self.send(Method::DELETE, &bucket_path(bucket), "", Vec::new())
            .await?;
        Ok(())
    }

    pub async fn list_buckets(&self) -> Result<Vec<BucketEntry>> {
        let response = self.send(Method::GET, "/", "", Vec::new()).await?;
        parse_bucket_entries(&response.bytes().await?)
    }

    pub async fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
        recursive: bool,
    ) -> Result<Vec<ListEntry>> {
        let mut entries = Vec::new();
        let mut continuation: Option<String> = None;
        loop {
            let mut query = vec![
                ("list-type", "2".to_owned()),
                ("prefix", prefix.to_owned()),
                ("max-keys", "1000".to_owned()),
            ];
            if !recursive {
                query.push(("delimiter", "/".to_owned()));
            }
            if let Some(token) = continuation.as_deref() {
                query.push(("continuation-token", token.to_owned()));
            }
            let query = encode_query(&query);
            let response = self
                .send(Method::GET, &bucket_path(bucket), &query, Vec::new())
                .await?;
            let page = parse_list_page(&response.bytes().await?)?;
            entries.extend(page.entries);
            if !page.truncated {
                break;
            }
            continuation = page.next_token;
            if continuation.is_none() {
                bail!("truncated list response omitted NextContinuationToken");
            }
        }
        Ok(entries)
    }

    pub async fn put_object(&self, path: &S3Path, body: Vec<u8>) -> Result<()> {
        self.send(Method::PUT, &object_path(path)?, "", body)
            .await?;
        Ok(())
    }

    pub async fn get_object(&self, path: &S3Path) -> Result<Vec<u8>> {
        let response = self
            .send(Method::GET, &object_path(path)?, "", Vec::new())
            .await?;
        Ok(response.bytes().await?.to_vec())
    }

    pub async fn stat_object(&self, path: &S3Path) -> Result<ObjectMetadata> {
        let response = self
            .send(Method::HEAD, &object_path(path)?, "", Vec::new())
            .await?;
        Ok(ObjectMetadata {
            size: response
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok())
                .unwrap_or_default(),
            etag: header_string(response.headers(), ETAG),
            modified: header_string(response.headers(), LAST_MODIFIED),
        })
    }

    pub async fn remove_object(&self, path: &S3Path) -> Result<()> {
        self.send(Method::DELETE, &object_path(path)?, "", Vec::new())
            .await?;
        Ok(())
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        query: &str,
        body: Vec<u8>,
    ) -> Result<Response> {
        ensure_success(self.request(method, path, query, body).await?).await
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        query: &str,
        body: Vec<u8>,
    ) -> Result<Response> {
        let signed = sign_request(
            method.as_str(),
            path,
            query,
            &self.host,
            &body,
            &self.credentials,
            &self.region,
        );
        let mut url = format!("{}{}", self.endpoint, path);
        if !query.is_empty() {
            url.push('?');
            url.push_str(query);
        }
        let response = self
            .http
            .request(method, url)
            .header(HOST, &self.host)
            .header("x-amz-date", signed.amz_date)
            .header("x-amz-content-sha256", signed.payload_hash)
            .header(AUTHORIZATION, signed.authorization)
            .body(body)
            .send()
            .await?;
        Ok(response)
    }
}

async fn ensure_success(response: Response) -> Result<Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = response.bytes().await.unwrap_or_default();
    let (code, message) = parse_error(&body);
    bail!(
        "S3 request failed: {} {}: {}",
        status.as_u16(),
        code.unwrap_or_else(|| status.to_string()),
        message.unwrap_or_else(|| "unknown error".to_owned())
    )
}

fn bucket_path(bucket: &str) -> String {
    format!("/{}", aws_encode(bucket, true))
}

fn object_path(path: &S3Path) -> Result<String> {
    if path.key.is_empty() {
        bail!("object path must include a key");
    }
    Ok(format!(
        "/{}/{}",
        aws_encode(&path.bucket, true),
        aws_encode(&path.key, false)
    ))
}

fn encode_query(values: &[(&str, String)]) -> String {
    let mut pairs = values
        .iter()
        .map(|(key, value)| format!("{}={}", aws_encode(key, true), aws_encode(value, true)))
        .collect::<Vec<_>>();
    pairs.sort_unstable();
    pairs.join("&")
}

fn aws_encode(value: &str, encode_slash: bool) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~')
            || (!encode_slash && byte == b'/')
        {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(b"0123456789ABCDEF"[(byte >> 4) as usize]));
            encoded.push(char::from(b"0123456789ABCDEF"[(byte & 0x0f) as usize]));
        }
    }
    encoded
}

#[derive(Debug)]
struct ListPage {
    entries: Vec<ListEntry>,
    truncated: bool,
    next_token: Option<String>,
}

fn parse_bucket_entries(xml: &[u8]) -> Result<Vec<BucketEntry>> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buckets = Vec::new();
    let mut in_bucket = false;
    let mut name = String::new();
    let mut creation_date = String::new();
    loop {
        match reader.read_event()? {
            Event::Start(start) if start.name().as_ref() == b"Bucket" => in_bucket = true,
            Event::Start(start) if in_bucket && start.name().as_ref() == b"Name" => {
                name = reader.read_text(start.name())?.into_owned();
            }
            Event::Start(start) if in_bucket && start.name().as_ref() == b"CreationDate" => {
                creation_date = reader.read_text(start.name())?.into_owned();
            }
            Event::End(end) if end.name().as_ref() == b"Bucket" => {
                buckets.push(BucketEntry {
                    name: std::mem::take(&mut name),
                    creation_date: std::mem::take(&mut creation_date),
                });
                in_bucket = false;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(buckets)
}

fn parse_list_page(xml: &[u8]) -> Result<ListPage> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut entries = Vec::new();
    let mut in_contents = false;
    let mut in_prefix = false;
    let mut key = String::new();
    let mut size = 0;
    let mut modified = String::new();
    let mut truncated = false;
    let mut next_token = None;
    loop {
        match reader.read_event()? {
            Event::Start(start) if start.name().as_ref() == b"Contents" => in_contents = true,
            Event::Start(start) if start.name().as_ref() == b"CommonPrefixes" => in_prefix = true,
            Event::Start(start) if in_contents && start.name().as_ref() == b"Key" => {
                key = reader.read_text(start.name())?.into_owned();
            }
            Event::Start(start) if in_contents && start.name().as_ref() == b"Size" => {
                size = reader.read_text(start.name())?.parse().unwrap_or_default();
            }
            Event::Start(start) if in_contents && start.name().as_ref() == b"LastModified" => {
                modified = reader.read_text(start.name())?.into_owned();
            }
            Event::Start(start) if in_prefix && start.name().as_ref() == b"Prefix" => {
                entries.push(ListEntry::Prefix(
                    reader.read_text(start.name())?.into_owned(),
                ));
            }
            Event::Start(start) if start.name().as_ref() == b"IsTruncated" => {
                truncated = reader.read_text(start.name())?.as_ref() == "true";
            }
            Event::Start(start) if start.name().as_ref() == b"NextContinuationToken" => {
                next_token = Some(reader.read_text(start.name())?.into_owned());
            }
            Event::End(end) if end.name().as_ref() == b"Contents" => {
                entries.push(ListEntry::Object {
                    key: std::mem::take(&mut key),
                    size,
                    modified: std::mem::take(&mut modified),
                });
                size = 0;
                in_contents = false;
            }
            Event::End(end) if end.name().as_ref() == b"CommonPrefixes" => in_prefix = false,
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(ListPage {
        entries,
        truncated,
        next_token,
    })
}

fn parse_error(xml: &[u8]) -> (Option<String>, Option<String>) {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut code = None;
    let mut message = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(start)) if start.name().as_ref() == b"Code" => {
                code = reader
                    .read_text(start.name())
                    .ok()
                    .map(std::borrow::Cow::into_owned);
            }
            Ok(Event::Start(start)) if start.name().as_ref() == b"Message" => {
                message = reader
                    .read_text(start.name())
                    .ok()
                    .map(std::borrow::Cow::into_owned);
            }
            Ok(Event::Eof) | Err(_) => break,
            Ok(_) => {}
        }
    }
    (code, message)
}

fn header_string(
    headers: &HeaderMap<HeaderValue>,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::{
        ListEntry, S3Path, aws_encode, encode_query, parse_bucket_entries, parse_list_page,
    };

    #[test]
    fn parses_remote_paths() {
        assert_eq!(
            S3Path::parse("s3://bucket/folder/file.txt").unwrap(),
            S3Path {
                bucket: "bucket".to_owned(),
                key: "folder/file.txt".to_owned(),
            }
        );
        assert!(S3Path::parse("bucket/file.txt").is_err());
    }

    #[test]
    fn uses_aws_uri_encoding() {
        assert_eq!(
            aws_encode("folder/a b+中.txt", false),
            "folder/a%20b%2B%E4%B8%AD.txt"
        );
        assert_eq!(
            encode_query(&[("prefix", "a b/".to_owned()), ("list-type", "2".to_owned())]),
            "list-type=2&prefix=a%20b%2F"
        );
    }

    #[test]
    fn parses_bucket_and_list_xml() {
        let buckets = parse_bucket_entries(
            br"<ListAllMyBucketsResult><Buckets><Bucket><Name>demo</Name><CreationDate>now</CreationDate></Bucket></Buckets></ListAllMyBucketsResult>",
        )
        .unwrap();
        assert_eq!(buckets[0].name, "demo");

        let page = parse_list_page(
            br"<ListBucketResult><Contents><Key>a.txt</Key><LastModified>now</LastModified><Size>3</Size></Contents><CommonPrefixes><Prefix>dir/</Prefix></CommonPrefixes><IsTruncated>false</IsTruncated></ListBucketResult>",
        )
        .unwrap();
        assert_eq!(page.entries.len(), 2);
        assert!(matches!(&page.entries[1], ListEntry::Prefix(value) if value == "dir/"));
    }
}

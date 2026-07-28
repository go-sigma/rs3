use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::SystemTime;

use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Request, State};
use axum::http::header::{
    ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, LAST_MODIFIED,
};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use chrono::{DateTime, SecondsFormat, Utc};
use percent_encoding::percent_decode_str;
use quick_xml::Reader;
use quick_xml::events::Event;

use crate::auth::{self, Credentials};
use crate::config::Config;
use crate::storage::{ObjectInfo, Storage, md5_etag};

const MAX_BODY_SIZE: usize = 5 * 1024 * 1024 * 1024;
const MAX_KEY_LENGTH: usize = 1024;
const MAX_BUCKET_LENGTH: usize = 63;
const EMPTY_MD5: &str = "d41d8cd98f00b204e9800998ecf8427e";

#[derive(Clone)]
pub struct AppState {
    config: Arc<Config>,
    credentials: Credentials,
    storage: Storage,
}

impl AppState {
    pub async fn new(config: Config) -> io::Result<Self> {
        let storage = Storage::new(config.data_dir.clone()).await?;
        let credentials = Credentials {
            access_key: config.access_key.clone(),
            secret_key: config.secret_key.clone(),
        };
        Ok(Self {
            config: Arc::new(config),
            credentials,
            storage,
        })
    }
}

pub async fn handle(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let is_head = parts.method == Method::HEAD;

    if parts.uri.path().starts_with("/_rs3/") {
        return finish_head(
            s3_error(
                StatusCode::NOT_FOUND,
                "NotFound",
                "Distributed mode is not supported",
            ),
            is_head,
        );
    }

    if auth::verify(&parts, &state.credentials).is_err() {
        return finish_head(
            s3_error(StatusCode::FORBIDDEN, "AccessDenied", "Invalid credentials"),
            is_head,
        );
    }

    let Ok(raw_body) = to_bytes(body, MAX_BODY_SIZE).await else {
        return finish_head(
            s3_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "EntityTooLarge",
                "Request body exceeds the configured limit",
            ),
            is_head,
        );
    };
    let Ok(body) = decode_request_body(&parts.headers, &raw_body) else {
        return finish_head(
            s3_error(
                StatusCode::BAD_REQUEST,
                "InvalidRequest",
                "Malformed aws-chunked request body",
            ),
            is_head,
        );
    };
    if auth::verify_payload(&parts, &body).is_err() {
        return finish_head(
            s3_error(
                StatusCode::FORBIDDEN,
                "XAmzContentSHA256Mismatch",
                "Request body hash does not match the signed payload hash",
            ),
            is_head,
        );
    }

    let Ok(decoded_path) = decode_path(parts.uri.path()) else {
        return finish_head(
            s3_error(
                StatusCode::BAD_REQUEST,
                "InvalidURI",
                "URI path is not valid UTF-8",
            ),
            is_head,
        );
    };
    let (bucket, key) = split_path(&decoded_path);
    if !bucket.is_empty() && !is_valid_bucket_name(bucket) {
        return finish_head(
            s3_error(
                StatusCode::BAD_REQUEST,
                "InvalidBucketName",
                "Bucket name is invalid",
            ),
            is_head,
        );
    }
    if !key.is_empty() && !is_valid_key(key) {
        return finish_head(
            s3_error(
                StatusCode::BAD_REQUEST,
                "InvalidKey",
                "Object key is invalid",
            ),
            is_head,
        );
    }

    let query = Query::new(parts.uri.query().unwrap_or_default());
    let response = dispatch(
        &state,
        &parts.method,
        &parts.headers,
        bucket,
        key,
        &query,
        &body,
    )
    .await;
    finish_head(response, is_head)
}

async fn dispatch(
    state: &AppState,
    method: &Method,
    headers: &HeaderMap,
    bucket: &str,
    key: &str,
    query: &Query,
    body: &[u8],
) -> Response {
    match (method, bucket.is_empty(), key.is_empty()) {
        (&Method::GET, true, _) => list_buckets(state).await,
        (&Method::PUT, false, true) => create_bucket(state, bucket).await,
        (&Method::HEAD, false, true) => head_bucket(state, bucket).await,
        (&Method::DELETE, false, true) => delete_bucket(state, bucket).await,
        (&Method::GET, false, true) => list_objects(state, bucket, query).await,
        (&Method::PUT, false, false) if query.has("uploadId") => {
            upload_part(state, bucket, key, query, body).await
        }
        (&Method::PUT, false, false) => put_object(state, bucket, key, body).await,
        (&Method::GET, false, false) => get_object(state, bucket, key, headers).await,
        (&Method::HEAD, false, false) => head_object(state, bucket, key).await,
        (&Method::DELETE, false, false) if query.has("uploadId") => {
            abort_multipart(state, bucket, key, query).await
        }
        (&Method::DELETE, false, false) => delete_object(state, bucket, key).await,
        (&Method::POST, false, true) if query.has("delete") => {
            delete_objects(state, bucket, body).await
        }
        (&Method::POST, false, false) if query.has("uploads") => {
            initiate_multipart(state, bucket, key).await
        }
        (&Method::POST, false, false) if query.has("uploadId") => {
            complete_multipart(state, bucket, key, query, body).await
        }
        (&Method::POST, _, _) => s3_error(
            StatusCode::BAD_REQUEST,
            "InvalidRequest",
            "Unknown POST operation",
        ),
        _ => s3_error(
            StatusCode::METHOD_NOT_ALLOWED,
            "MethodNotAllowed",
            "Method not allowed",
        ),
    }
}

async fn create_bucket(state: &AppState, bucket: &str) -> Response {
    match state.storage.create_bucket(bucket).await {
        Ok(()) => empty_response(StatusCode::OK),
        Err(error) => internal_error("Cannot create bucket", error),
    }
}

async fn head_bucket(state: &AppState, bucket: &str) -> Response {
    if state.storage.bucket_exists(bucket).await {
        empty_response(StatusCode::OK)
    } else {
        s3_error(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "Bucket does not exist",
        )
    }
}

async fn delete_bucket(state: &AppState, bucket: &str) -> Response {
    match state.storage.delete_bucket(bucket).await {
        Ok(()) => empty_response(StatusCode::NO_CONTENT),
        Err(error) if error.kind() == io::ErrorKind::NotFound => s3_error(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "Bucket does not exist",
        ),
        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => s3_error(
            StatusCode::CONFLICT,
            "BucketNotEmpty",
            "Bucket is not empty",
        ),
        Err(error) => internal_error("Cannot delete bucket", error),
    }
}

async fn list_buckets(state: &AppState) -> Response {
    let buckets = match state.storage.list_buckets().await {
        Ok(buckets) => buckets,
        Err(error) => return internal_error("Cannot list buckets", error),
    };

    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListAllMyBucketsResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
         <Owner><ID>{0}</ID><DisplayName>{0}</DisplayName></Owner><Buckets>",
        xml_escape(&state.config.access_key)
    );
    for bucket in buckets {
        xml.push_str("<Bucket><Name>");
        xml.push_str(&xml_escape(&bucket.key));
        xml.push_str("</Name><CreationDate>");
        xml.push_str(&iso8601(bucket.modified));
        xml.push_str("</CreationDate></Bucket>");
    }
    xml.push_str("</Buckets></ListAllMyBucketsResult>");
    xml_response(StatusCode::OK, xml)
}

async fn put_object(state: &AppState, bucket: &str, key: &str, body: &[u8]) -> Response {
    if !state.storage.bucket_exists(bucket).await {
        return s3_error(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "Bucket does not exist",
        );
    }
    match state.storage.put_object(bucket, key, body).await {
        Ok(()) => response_builder(StatusCode::OK)
            .header(ETAG, md5_etag(body))
            .body(Body::empty())
            .expect("valid response"),
        Err(error) => internal_error("Cannot write object", error),
    }
}

async fn get_object(state: &AppState, bucket: &str, key: &str, headers: &HeaderMap) -> Response {
    let data = match state.storage.read_object(bucket, key).await {
        Ok(data) => data,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return s3_error(StatusCode::NOT_FOUND, "NoSuchKey", "Object not found");
        }
        Err(error) => return internal_error("Cannot read object", error),
    };
    let metadata = match state.storage.object_metadata(bucket, key).await {
        Ok(metadata) => metadata,
        Err(error) => return internal_error("Cannot stat object", error),
    };

    let full_len = data.len() as u64;
    let builder = object_response_builder(StatusCode::OK, &data, &metadata);

    if let Some(range) = headers
        .get("range")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_range(value, full_len))
    {
        let body = data[range.0 as usize..=range.1 as usize].to_vec();
        return object_response_builder(StatusCode::PARTIAL_CONTENT, &data, &metadata)
            .header(
                CONTENT_RANGE,
                format!("bytes {}-{}/{}", range.0, range.1, full_len),
            )
            .header(CONTENT_LENGTH, body.len().to_string())
            .body(Body::from(body))
            .expect("valid response");
    }

    builder
        .header(CONTENT_LENGTH, data.len().to_string())
        .body(Body::from(data))
        .expect("valid response")
}

async fn head_object(state: &AppState, bucket: &str, key: &str) -> Response {
    let metadata = match state.storage.object_metadata(bucket, key).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return s3_error(StatusCode::NOT_FOUND, "NoSuchKey", "Object not found");
        }
        Err(error) => return internal_error("Cannot stat object", error),
    };
    let data = match state.storage.read_object(bucket, key).await {
        Ok(data) => data,
        Err(error) => return internal_error("Cannot read object", error),
    };
    object_response_builder(StatusCode::OK, &data, &metadata)
        .header(CONTENT_LENGTH, metadata.len().to_string())
        .body(Body::empty())
        .expect("valid response")
}

async fn delete_object(state: &AppState, bucket: &str, key: &str) -> Response {
    match state.storage.delete_object(bucket, key).await {
        Ok(()) => empty_response(StatusCode::NO_CONTENT),
        Err(error) => internal_error("Cannot delete object", error),
    }
}

async fn list_objects(state: &AppState, bucket: &str, query: &Query) -> Response {
    if !state.storage.bucket_exists(bucket).await {
        return s3_error(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "Bucket does not exist",
        );
    }
    let objects = match state.storage.list_objects(bucket).await {
        Ok(objects) => objects,
        Err(error) => return internal_error("Cannot list objects", error),
    };
    let prefix = query.get("prefix").unwrap_or_default();
    let delimiter = query.get("delimiter").filter(|value| !value.is_empty());
    let continuation = query.get("continuation-token");
    let max_keys = query
        .get("max-keys")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1000);

    let entries = make_list_entries(objects, prefix, delimiter, continuation);
    let truncated = entries.len() > max_keys;
    let selected = entries.iter().take(max_keys).collect::<Vec<_>>();
    let next_token = if truncated {
        selected.last().map(|entry| entry.marker())
    } else {
        None
    };

    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
         <Name>{}</Name><Prefix>{}</Prefix><MaxKeys>{}</MaxKeys>",
        xml_escape(bucket),
        xml_escape(prefix),
        max_keys
    );
    for entry in &selected {
        match entry {
            ListEntry::Object(object) => {
                xml.push_str("<Contents><Key>");
                xml.push_str(&xml_escape(&object.key));
                xml.push_str("</Key><LastModified>");
                xml.push_str(&iso8601(object.modified));
                xml.push_str("</LastModified><ETag>\"");
                xml.push_str(EMPTY_MD5);
                xml.push_str("</ETag><Size>");
                xml.push_str(&object.size.to_string());
                xml.push_str("</Size><StorageClass>STANDARD</StorageClass></Contents>");
            }
            ListEntry::Prefix { value, .. } => {
                xml.push_str("<CommonPrefixes><Prefix>");
                xml.push_str(&xml_escape(value));
                xml.push_str("</Prefix></CommonPrefixes>");
            }
        }
    }
    xml.push_str(&format!(
        "<KeyCount>{}</KeyCount><IsTruncated>{}</IsTruncated>",
        selected.len(),
        truncated
    ));
    if let Some(token) = next_token {
        xml.push_str("<NextContinuationToken>");
        xml.push_str(&xml_escape(token));
        xml.push_str("</NextContinuationToken>");
    }
    xml.push_str("</ListBucketResult>");
    xml_response(StatusCode::OK, xml)
}

async fn delete_objects(state: &AppState, bucket: &str, body: &[u8]) -> Response {
    if !state.storage.bucket_exists(bucket).await {
        return s3_error(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "Bucket does not exist",
        );
    }
    let Ok(keys) = parse_xml_values(body, b"Key") else {
        return s3_error(
            StatusCode::BAD_REQUEST,
            "MalformedXML",
            "Delete request XML is invalid",
        );
    };
    let mut xml = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                   <DeleteResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">"
        .to_owned();
    for key in keys {
        if !is_valid_key(&key) {
            continue;
        }
        if let Err(error) = state.storage.delete_object(bucket, &key).await {
            return internal_error("Cannot delete object", error);
        }
        xml.push_str("<Deleted><Key>");
        xml.push_str(&xml_escape(&key));
        xml.push_str("</Key></Deleted>");
    }
    xml.push_str("</DeleteResult>");
    xml_response(StatusCode::OK, xml)
}

async fn initiate_multipart(state: &AppState, bucket: &str, key: &str) -> Response {
    if !state.storage.bucket_exists(bucket).await {
        return s3_error(
            StatusCode::NOT_FOUND,
            "NoSuchBucket",
            "Bucket does not exist",
        );
    }
    match state.storage.create_upload(bucket, key).await {
        Ok(upload_id) => xml_response(
            StatusCode::OK,
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                 <InitiateMultipartUploadResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                 <Bucket>{}</Bucket><Key>{}</Key><UploadId>{}</UploadId>\
                 </InitiateMultipartUploadResult>",
                xml_escape(bucket),
                xml_escape(key),
                upload_id
            ),
        ),
        Err(error) => internal_error("Cannot initiate multipart upload", error),
    }
}

async fn upload_part(
    state: &AppState,
    bucket: &str,
    key: &str,
    query: &Query,
    body: &[u8],
) -> Response {
    let Some(upload_id) = query.get("uploadId") else {
        return invalid_request("Missing uploadId");
    };
    let Some(part_number) = query
        .get("partNumber")
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|number| (1..=10_000).contains(number))
    else {
        return invalid_request("Invalid partNumber");
    };
    if !is_valid_upload_id(upload_id) {
        return invalid_request("Invalid uploadId");
    }
    match state
        .storage
        .put_part(upload_id, bucket, key, part_number, body)
        .await
    {
        Ok(etag) => response_builder(StatusCode::OK)
            .header(ETAG, etag)
            .body(Body::empty())
            .expect("valid response"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => s3_error(
            StatusCode::NOT_FOUND,
            "NoSuchUpload",
            "Multipart upload not found",
        ),
        Err(error) => internal_error("Cannot write multipart part", error),
    }
}

async fn complete_multipart(
    state: &AppState,
    bucket: &str,
    key: &str,
    query: &Query,
    body: &[u8],
) -> Response {
    let Some(upload_id) = query.get("uploadId") else {
        return invalid_request("Missing uploadId");
    };
    if !is_valid_upload_id(upload_id) {
        return invalid_request("Invalid uploadId");
    }
    let requested_parts = parse_xml_values(body, b"PartNumber")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| value.parse::<u32>().ok())
        .collect::<Vec<_>>();
    match state
        .storage
        .complete_upload(upload_id, bucket, key, &requested_parts)
        .await
    {
        Ok(completed) => xml_response(
            StatusCode::OK,
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                 <CompleteMultipartUploadResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                 <Bucket>{}</Bucket><Key>{}</Key><ETag>\"{}\"</ETag>\
                 </CompleteMultipartUploadResult>",
                xml_escape(bucket),
                xml_escape(key),
                completed.etag
            ),
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => s3_error(
            StatusCode::NOT_FOUND,
            "NoSuchUpload",
            "Multipart upload not found",
        ),
        Err(error) => internal_error("Cannot complete multipart upload", error),
    }
}

async fn abort_multipart(state: &AppState, bucket: &str, key: &str, query: &Query) -> Response {
    let Some(upload_id) = query.get("uploadId") else {
        return invalid_request("Missing uploadId");
    };
    if !is_valid_upload_id(upload_id) {
        return invalid_request("Invalid uploadId");
    }
    match state.storage.abort_upload(upload_id, bucket, key).await {
        Ok(()) => empty_response(StatusCode::NO_CONTENT),
        Err(error) if error.kind() == io::ErrorKind::NotFound => s3_error(
            StatusCode::NOT_FOUND,
            "NoSuchUpload",
            "Multipart upload not found",
        ),
        Err(error) => internal_error("Cannot abort multipart upload", error),
    }
}

#[derive(Debug)]
enum ListEntry {
    Object(ObjectInfo),
    Prefix { value: String, marker: String },
}

impl ListEntry {
    fn marker(&self) -> &str {
        match self {
            Self::Object(object) => &object.key,
            Self::Prefix { marker, .. } => marker,
        }
    }
}

fn make_list_entries(
    objects: Vec<ObjectInfo>,
    prefix: &str,
    delimiter: Option<&str>,
    continuation: Option<&str>,
) -> Vec<ListEntry> {
    let mut entries = Vec::new();
    let mut prefix_indexes = HashMap::<String, usize>::new();
    for object in objects {
        if !object.key.starts_with(prefix)
            || continuation.is_some_and(|token| object.key.as_str() <= token)
        {
            continue;
        }
        if let Some(delimiter) = delimiter {
            let remainder = &object.key[prefix.len()..];
            if let Some(index) = remainder.find(delimiter) {
                let common = object.key[..prefix.len() + index + delimiter.len()].to_owned();
                if let Some(entry_index) = prefix_indexes.get(&common).copied() {
                    if let ListEntry::Prefix { marker, .. } = &mut entries[entry_index] {
                        marker.clone_from(&object.key);
                    }
                } else {
                    prefix_indexes.insert(common.clone(), entries.len());
                    entries.push(ListEntry::Prefix {
                        value: common,
                        marker: object.key,
                    });
                }
                continue;
            }
        }
        entries.push(ListEntry::Object(object));
    }
    entries
}

#[derive(Debug)]
struct Query {
    values: Vec<(String, String)>,
}

impl Query {
    fn new(raw: &str) -> Self {
        Self {
            values: url::form_urlencoded::parse(raw.as_bytes())
                .into_owned()
                .collect(),
        }
    }

    fn has(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

fn decode_path(raw_path: &str) -> Result<String, ()> {
    percent_decode_str(raw_path.trim_start_matches('/'))
        .decode_utf8()
        .map(std::borrow::Cow::into_owned)
        .map_err(|_| ())
}

fn split_path(path: &str) -> (&str, &str) {
    path.split_once('/').unwrap_or((path, ""))
}

fn is_valid_bucket_name(name: &str) -> bool {
    (3..=MAX_BUCKET_LENGTH).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-.".contains(&byte))
        && !name.starts_with(['-', '.'])
        && !name.ends_with(['-', '.'])
}

fn is_valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_KEY_LENGTH
        && !key.starts_with('/')
        && !key.contains("..")
        && !key.chars().any(char::is_control)
}

fn is_valid_upload_id(upload_id: &str) -> bool {
    !upload_id.is_empty()
        && upload_id.len() <= 64
        && upload_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn parse_range(header: &str, file_size: u64) -> Option<(u64, u64)> {
    let range = header.strip_prefix("bytes=")?;
    let (start, end) = range.split_once('-')?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?;
        if suffix == 0 || file_size == 0 {
            return None;
        }
        let length = suffix.min(file_size);
        return Some((file_size - length, file_size - 1));
    }
    if file_size == 0 {
        return None;
    }
    let start = start.parse::<u64>().ok()?;
    let end = if end.is_empty() {
        file_size - 1
    } else {
        end.parse::<u64>().ok()?
    };
    (start <= end && end < file_size).then_some((start, end))
}

fn decode_request_body(headers: &HeaderMap, body: &Bytes) -> Result<Vec<u8>, ()> {
    let aws_chunked = headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|item| item.trim() == "aws-chunked"))
        || headers
            .get("x-amz-content-sha256")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("STREAMING-"));
    if !aws_chunked {
        return Ok(body.to_vec());
    }

    let mut decoded = Vec::new();
    let mut remaining = body.as_ref();
    loop {
        let line_end = find_bytes(remaining, b"\r\n").ok_or(())?;
        let header = std::str::from_utf8(&remaining[..line_end]).map_err(|_| ())?;
        let size =
            usize::from_str_radix(header.split(';').next().ok_or(())?, 16).map_err(|_| ())?;
        remaining = &remaining[line_end + 2..];
        if size == 0 {
            break;
        }
        if remaining.len() < size + 2 || &remaining[size..size + 2] != b"\r\n" {
            return Err(());
        }
        decoded.extend_from_slice(&remaining[..size]);
        remaining = &remaining[size + 2..];
    }
    if headers
        .get("x-amz-decoded-content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|expected| expected != decoded.len())
    {
        return Err(());
    }
    Ok(decoded)
}

fn parse_xml_values(body: &[u8], wanted: &[u8]) -> Result<Vec<String>, ()> {
    let mut reader = Reader::from_reader(body);
    reader.config_mut().trim_text(true);
    let mut values = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(start)) if start.name().as_ref() == wanted => {
                let value = reader.read_text(start.name()).map_err(|_| ())?.into_owned();
                values.push(value);
            }
            Ok(Event::Eof) => return Ok(values),
            Ok(_) => {}
            Err(_) => return Err(()),
        }
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn finish_head(mut response: Response, is_head: bool) -> Response {
    if is_head {
        *response.body_mut() = Body::empty();
    }
    response
}

fn response_builder(status: StatusCode) -> axum::http::response::Builder {
    Response::builder().status(status)
}

fn object_response_builder(
    status: StatusCode,
    data: &[u8],
    metadata: &std::fs::Metadata,
) -> axum::http::response::Builder {
    response_builder(status)
        .header(ACCEPT_RANGES, "bytes")
        .header(ETAG, md5_etag(data))
        .header(
            LAST_MODIFIED,
            http_date(metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH)),
        )
}

fn empty_response(status: StatusCode) -> Response {
    response_builder(status)
        .body(Body::empty())
        .expect("valid response")
}

fn xml_response(status: StatusCode, xml: String) -> Response {
    response_builder(status)
        .header(CONTENT_TYPE, "application/xml")
        .header(CONTENT_LENGTH, xml.len().to_string())
        .body(Body::from(xml))
        .expect("valid response")
}

fn s3_error(status: StatusCode, code: &str, message: &str) -> Response {
    xml_response(
        status,
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <Error><Code>{}</Code><Message>{}</Message></Error>",
            xml_escape(code),
            xml_escape(message)
        ),
    )
}

fn invalid_request(message: &str) -> Response {
    s3_error(StatusCode::BAD_REQUEST, "InvalidRequest", message)
}

fn internal_error(message: &str, error: io::Error) -> Response {
    tracing::error!(%error, "{message}");
    s3_error(StatusCode::INTERNAL_SERVER_ERROR, "InternalError", message)
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn iso8601(time: SystemTime) -> String {
    DateTime::<Utc>::from(time).to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn http_date(time: SystemTime) -> String {
    DateTime::<Utc>::from(time)
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{decode_request_body, is_valid_bucket_name, is_valid_key, parse_range};
    use axum::http::{HeaderMap, HeaderValue};
    use bytes::Bytes;

    #[test]
    fn validates_bucket_names() {
        assert!(is_valid_bucket_name("my-bucket"));
        assert!(!is_valid_bucket_name("ab"));
        assert!(!is_valid_bucket_name("MyBucket"));
        assert!(!is_valid_bucket_name("my_bucket"));
    }

    #[test]
    fn blocks_unsafe_keys() {
        assert!(is_valid_key("folder/file.txt"));
        assert!(!is_valid_key("../etc/passwd"));
        assert!(!is_valid_key("a..b"));
    }

    #[test]
    fn parses_byte_ranges() {
        assert_eq!(parse_range("bytes=0-4", 20), Some((0, 4)));
        assert_eq!(parse_range("bytes=15-", 20), Some((15, 19)));
        assert_eq!(parse_range("bytes=-5", 20), Some((15, 19)));
        assert_eq!(parse_range("bytes=20-21", 20), None);
    }

    #[test]
    fn decodes_aws_chunked_body() {
        let body = Bytes::from_static(
            b"5;chunk-signature=abc\r\nhello\r\n6;chunk-signature=def\r\n world\r\n0;chunk-signature=ghi\r\n\r\n",
        );
        let mut headers = HeaderMap::new();
        headers.insert("content-encoding", HeaderValue::from_static("aws-chunked"));
        headers.insert(
            "x-amz-decoded-content-length",
            HeaderValue::from_static("11"),
        );
        assert_eq!(
            decode_request_body(&headers, &body).unwrap(),
            b"hello world"
        );
    }
}

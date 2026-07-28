# API Reference

`rs3` implements a focused subset of the path-style AWS S3 REST API.

## Authentication

Requests use AWS Signature Version 4 header authentication:

```text
Authorization: AWS4-HMAC-SHA256
  Credential={access_key}/{date}/{region}/s3/aws4_request,
  SignedHeaders={signed_headers},
  Signature={signature}
```

The configured access key must match the credential scope. Signed headers are
canonicalized and the signature is compared in constant time.

`x-amz-content-sha256` may contain a SHA-256 digest, `UNSIGNED-PAYLOAD`, or an
AWS streaming payload marker. A concrete digest is checked after body decoding.
Presigned URL authentication is not supported.

## Bucket Operations

| Operation     | Request                     | Success        |
| ------------- | --------------------------- | -------------- |
| ListBuckets   | `GET /`                     | `200` with XML |
| CreateBucket  | `PUT /{bucket}`             | `200`          |
| HeadBucket    | `HEAD /{bucket}`            | `200`          |
| DeleteBucket  | `DELETE /{bucket}`          | `204`          |
| ListObjectsV2 | `GET /{bucket}?list-type=2` | `200` with XML |

Deleting a non-empty bucket returns `BucketNotEmpty`. Listing supports:

- `prefix`
- `delimiter`
- `max-keys`, default `1000`
- `continuation-token`

## Object Operations

| Operation     | Request                  | Success                     |
| ------------- | ------------------------ | --------------------------- |
| PutObject     | `PUT /{bucket}/{key}`    | `200` with `ETag`           |
| GetObject     | `GET /{bucket}/{key}`    | `200` with object body      |
| HeadObject    | `HEAD /{bucket}/{key}`   | `200` with metadata headers |
| DeleteObject  | `DELETE /{bucket}/{key}` | `204`                       |
| DeleteObjects | `POST /{bucket}?delete`  | `200` with XML              |

Object metadata includes `Content-Length`, `ETag`, `Last-Modified`, and
`Accept-Ranges` where applicable. User-defined metadata, content types,
checksums other than ETag, conditional requests, and object tags are not
persisted.

DeleteObject is idempotent and returns `204` for a missing key.

### Range Requests

GetObject accepts one byte range:

```text
Range: bytes=0-1023
Range: bytes=1024-
Range: bytes=-512
```

A valid range returns `206 Partial Content` with `Content-Range`. Multiple
ranges are not supported.

### Batch Delete

DeleteObjects accepts the standard XML shape:

```xml
<Delete>
  <Object><Key>first.txt</Key></Object>
  <Object><Key>second.txt</Key></Object>
</Delete>
```

The response lists successfully deleted keys. Missing objects are treated as
success.

## Multipart Uploads

| Operation               | Request                                            | Success               |
| ----------------------- | -------------------------------------------------- | --------------------- |
| InitiateMultipartUpload | `POST /{bucket}/{key}?uploads`                     | `200` with `UploadId` |
| UploadPart              | `PUT /{bucket}/{key}?uploadId={id}&partNumber={n}` | `200` with `ETag`     |
| CompleteMultipartUpload | `POST /{bucket}/{key}?uploadId={id}`               | `200` with XML        |
| AbortMultipartUpload    | `DELETE /{bucket}/{key}?uploadId={id}`             | `204`                 |

Part numbers start at 1. Completion accepts the part numbers from the standard
completion XML and concatenates them in numeric order. If the request omits a
part list, all uploaded numeric parts are used.

The server does not enforce AWS minimum part sizes or maximum part counts.
ListMultipartUploads, ListParts, UploadPartCopy, and checksum negotiation are
not supported.

## Naming Rules

Bucket names:

- Are 3 to 63 characters
- Start and end with a lowercase ASCII letter or digit
- Contain only lowercase ASCII letters, digits, dots, and hyphens

Object keys:

- Are non-empty
- Are at most 1024 bytes
- Must not contain `..` anywhere
- Must not contain control characters

Path-style addressing is expected. Virtual-hosted bucket addressing is not
implemented.

## Error Responses

Protocol errors use S3-style XML:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<Error>
  <Code>NoSuchKey</Code>
  <Message>Object not found</Message>
</Error>
```

Common errors include:

| Code                        | Status | Meaning                                           |
| --------------------------- | ------ | ------------------------------------------------- |
| `InvalidRequest`            | `400`  | Malformed or unsupported request                  |
| `InvalidBucketName`         | `400`  | Bucket name validation failed                     |
| `InvalidKey`                | `400`  | Object key validation failed                      |
| `AccessDenied`              | `403`  | Authentication failed                             |
| `XAmzContentSHA256Mismatch` | `403`  | Payload hash mismatch                             |
| `NoSuchBucket`              | `404`  | Bucket does not exist                             |
| `NoSuchKey`                 | `404`  | Object does not exist                             |
| `NoSuchUpload`              | `404`  | Multipart upload does not exist or does not match |
| `BucketNotEmpty`            | `409`  | Bucket still contains objects                     |
| `EntityTooLarge`            | `413`  | Body exceeds the request limit                    |
| `MethodNotAllowed`          | `405`  | Method is unsupported for the resource            |
| `InternalError`             | `500`  | Unexpected storage failure                        |

## Limits and Omissions

| Limit                      | Value              |
| -------------------------- | ------------------ |
| Request body               | 5 GiB              |
| Object key                 | 1024 bytes         |
| Bucket name                | 3 to 63 characters |
| Default ListObjectsV2 page | 1000 entries       |

Notable omissions include ACLs, bucket policies, multiple users, versioning,
lifecycle rules, replication, server-side encryption, website hosting,
presigned URLs, object copying, and distributed storage.

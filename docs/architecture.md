# Architecture

`rs3` is an asynchronous HTTP service built with Axum and Tokio. It exposes a
focused subset of the S3 REST API and stores objects directly on a local
filesystem.

## Request Flow

```text
TCP listener
    |
    v
Axum fallback route
    |
    v
server::handle
    |-- verify AWS SigV4 headers
    |-- buffer and decode the request body
    |-- verify the payload hash
    |-- decode and validate bucket/key paths
    |-- dispatch the S3 operation
    `-- serialize an HTTP or S3 XML response
```

The Axum fallback route sends every request to `server::handle`. The handler
authenticates before reading or mutating storage, then dispatches based on the
HTTP method, bucket/key path, and query parameters.

## Modules

| Module | Responsibility |
| --- | --- |
| `main.rs` | Logging, configuration parsing, listener startup, and graceful shutdown |
| `config.rs` | CLI flags, environment variables, and listen address |
| `auth.rs` | AWS SigV4 signing, canonicalization, verification, and payload checks |
| `server.rs` | Request validation, operation dispatch, XML responses, and range handling |
| `storage.rs` | Bucket, object, listing, and multipart filesystem operations |
| `client.rs` | Signed HTTP client and XML parsing used by `rs3-cli` |
| `bin/rs3-cli.rs` | User-facing bucket and object commands |

`AppState` is cloned into Axum handlers and contains immutable shared
configuration, credentials, and a lightweight `Storage` handle.

## Authentication

The server supports one configured access key and secret key. For each request:

1. Parse the `Authorization` header and credential scope
2. Canonicalize the method, URI, query, signed headers, and payload hash
3. Hash the canonical request
4. Derive the signing key from date, region, service, and secret
5. Calculate the expected HMAC-SHA256 signature
6. Compare signatures in constant time
7. Verify the body hash when a concrete SHA-256 value is supplied

`UNSIGNED-PAYLOAD` and AWS streaming payload markers are accepted. Streaming
chunk framing is decoded before the request reaches an operation handler.

## Storage Model

```text
data/
├── bucket-a/
│   ├── object.txt
│   └── nested/
│       └── object.bin
├── bucket-b/
└── .uploads/
    └── upload-id/
        ├── .meta
        ├── 1
        └── 2
```

- Buckets are directories immediately below the configured data root
- Object keys are relative file paths inside a bucket
- Keys ending in `/` use a hidden `.folder_marker` file
- Multipart state is isolated under `.uploads/{upload_id}`
- Hidden temporary files are excluded from object listings

Bucket and key validation rejects traversal components before storage paths are
constructed.

## Writes and Multipart Uploads

Normal object writes go to a unique temporary file in the destination
directory and are renamed into place after a successful flush. Readers
therefore do not observe a partially written replacement.

Multipart uploads store each part as a numbered file. UploadPartCopy reads a
source object, applies an optional byte range, and stores the result as a
numbered part in the same way as UploadPart. Completion concatenates the
selected parts into another temporary file, renames it into the object path,
and removes the upload directory. Abort removes the upload directory without
creating an object.

## Concurrency

Tokio runs request handlers concurrently. Async filesystem APIs are used for
direct operations, while recursive directory traversal is moved to
`spawn_blocking`.

There is no cross-process locking or transaction journal. Concurrent writes to
the same key are atomic at rename time, but last completion wins. Deployment
must ensure that only one server process owns a data directory.

## Memory and Body Limits

Axum buffers each request body before dispatch. The current maximum is 5 GiB,
so large requests can require substantial memory. Responses also read complete
objects into memory before sending them.

This design keeps the implementation compact but is not appropriate for
unbounded or high-concurrency large-object workloads.

## Shutdown and Errors

On Unix, the server waits for `SIGINT` or `SIGTERM` and asks Axum to finish
gracefully. Other platforms use the Ctrl-C signal.

Protocol errors are serialized as S3-compatible XML where applicable.
Unexpected filesystem failures are logged and returned as `InternalError`.

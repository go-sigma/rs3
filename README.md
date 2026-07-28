# rs3

`rs3` is a small S3-compatible object storage server written in Rust. It uses
the local filesystem for storage, supports AWS Signature Version 4, and ships
with a companion command-line client.

This project was inspired by [Lulzx/zs3](https://github.com/Lulzx/zs3) and
rewritten in Rust with the assistance of large language models.

The project is intended for local development, CI artifact storage, small
self-hosted installations, and learning. It is not a complete replacement for
AWS S3 and does not provide distributed storage or multi-user authorization.

## Features

- AWS Signature Version 4 header authentication
- List, create, inspect, and delete buckets
- Put, get, inspect, and delete objects
- ListObjectsV2 with prefixes, delimiters, limits, and continuation tokens
- Byte range requests, including suffix ranges
- DeleteObjects batch deletion
- Multipart upload initiation, part upload, completion, and abort
- AWS streaming chunk body decoding
- Atomic object replacement through temporary files and rename
- `rs3-cli` for common bucket and object operations
- Multi-stage Docker image running as a non-root user

See [API reference](docs/api.md) for the supported S3 surface and known limits.

## Requirements

- Rust 1.85 or newer
- Cargo

## Quick Start

Build and start the server:

```bash
cargo build --release
cargo run --release
```

The server listens on `0.0.0.0:9000`, stores data under `./data`, and uses
`minioadmin:minioadmin` by default. Set explicit credentials outside local
development:

```bash
RS3_ACCESS_KEY=local-access \
RS3_SECRET_KEY=local-secret \
cargo run --release
```

Use the bundled `rs3-cli` against the server:

```bash
export RS3_ENDPOINT=http://localhost:9000
export RS3_ACCESS_KEY=local-access
export RS3_SECRET_KEY=local-secret

cargo run --release --bin rs3-cli -- mb --ensure s3://mybucket
cargo run --release --bin rs3-cli -- cp ./file.txt s3://mybucket/
cargo run --release --bin rs3-cli -- ls s3://mybucket/
```

## Configuration

Flags and equivalent environment variables are available:

| Flag           | Environment variable | Default      |
| -------------- | -------------------- | ------------ |
| `--host`       | `RS3_HOST`           | `0.0.0.0`    |
| `--port`       | `RS3_PORT`           | `9000`       |
| `--data-dir`   | `RS3_DATA_DIR`       | `data`       |
| `--access-key` | `RS3_ACCESS_KEY`     | `minioadmin` |
| `--secret-key` | `RS3_SECRET_KEY`     | `minioadmin` |

CLI flags take precedence over environment variables. See
[configuration](docs/configuration.md) for additional examples.

## Command-Line Client

`rs3-cli` accepts remote paths in `s3://bucket/key` form. Its connection
settings use `RS3_ENDPOINT`, `RS3_ACCESS_KEY`, `RS3_SECRET_KEY`, and
`RS3_REGION`.

```bash
export RS3_ENDPOINT=http://localhost:9000
export RS3_ACCESS_KEY=local-access
export RS3_SECRET_KEY=local-secret

cargo run --release --bin rs3-cli -- mb --ensure s3://mybucket
cargo run --release --bin rs3-cli -- cp ./photo.jpg s3://mybucket/photos/
cargo run --release --bin rs3-cli -- ls --recursive s3://mybucket/
cargo run --release --bin rs3-cli -- stat s3://mybucket/photos/photo.jpg
cargo run --release --bin rs3-cli -- cp s3://mybucket/photos/photo.jpg ./photo.jpg
cargo run --release --bin rs3-cli -- rm s3://mybucket/photos/photo.jpg
cargo run --release --bin rs3-cli -- rb s3://mybucket
```

The client handles one object per `cp` invocation. Recursive transfers,
server-side copies, policies, identities, lifecycle rules, and replication are
not implemented.

## Docker

Build and run the included multi-stage image:

```bash
docker build --tag rs3:local .
docker run --rm \
  --name rs3 \
  --publish 9000:9000 \
  --env RS3_ACCESS_KEY=local-access \
  --env RS3_SECRET_KEY=local-secret \
  --volume rs3-data:/var/lib/rs3/data \
  rs3:local
```

The image contains `/usr/local/bin/rs3` and `/usr/local/bin/rs3-cli`, runs as
`10001:10001`, and persists objects under `/var/lib/rs3/data`.

See [deployment](docs/deployment.md) for native binary, backup, and container
examples.

## Development

Run formatting, tests, and static analysis:

```bash
cargo fmt --all -- --check
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
```

Build every binary in release mode:

```bash
cargo build --locked --release --bins
```

## Project Layout

```text
.
├── Cargo.toml              # Package metadata and dependencies
├── Cargo.lock              # Reproducible dependency versions
├── Dockerfile              # Multi-stage production image
├── README.md               # Project overview and quick start
├── docs/
│   ├── api.md              # Supported S3 operations and limits
│   ├── architecture.md     # Runtime and storage design
│   ├── configuration.md    # Flags and environment variables
│   └── deployment.md       # Production deployment guidance
└── src/
    ├── auth.rs             # SigV4 signing and verification
    ├── client.rs           # S3 client used by rs3-cli
    ├── config.rs           # Server configuration
    ├── lib.rs              # Library module exports
    ├── main.rs             # Server binary
    ├── server.rs           # HTTP routing and S3 handlers
    ├── storage.rs          # Filesystem and multipart storage
    └── bin/
        └── rs3-cli.rs      # Companion command-line client
```

## Storage Layout

Buckets and objects map directly to directories and files:

```text
data/
├── bucket/
│   └── path/to/object
└── .uploads/
    └── upload-id/
        ├── .meta
        ├── 1
        └── 2
```

Multipart state is removed after completion or abort. Back up the data
directory as a normal filesystem tree while writes are stopped or externally
coordinated.

## Documentation

- [API reference](docs/api.md)
- [Architecture](docs/architecture.md)
- [Configuration](docs/configuration.md)
- [Deployment](docs/deployment.md)

## License

[MIT](LICENSE)

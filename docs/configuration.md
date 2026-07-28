# Configuration

`rs3` reads server settings from command-line flags and environment variables.
Flags take precedence over environment values.

## Settings

| Flag | Environment variable | Default | Description |
| --- | --- | --- | --- |
| `--host` | `RS3_HOST` | `0.0.0.0` | IP address to listen on |
| `--port` | `RS3_PORT` | `9000` | HTTP port |
| `--data-dir` | `RS3_DATA_DIR` | `data` | Bucket, object, and multipart root |
| `--access-key` | `RS3_ACCESS_KEY` | `minioadmin` | AWS access key ID |
| `--secret-key` | `RS3_SECRET_KEY` | `minioadmin` | AWS secret access key |

List the current binary's options with:

```bash
cargo run --release -- --help
```

## Command-Line Example

```bash
cargo run --release -- \
  --host 127.0.0.1 \
  --port 9000 \
  --data-dir ./var/data \
  --access-key local-access \
  --secret-key local-secret
```

## Environment Example

```bash
export RS3_HOST=127.0.0.1
export RS3_PORT=9000
export RS3_DATA_DIR=./var/data
export RS3_ACCESS_KEY=local-access
export RS3_SECRET_KEY=local-secret
cargo run --release
```

The built-in credentials are intended only for local development. Supply
unique credentials through a secret manager or protected service environment
in deployed installations.

## Client Settings

The companion `rs3-cli` has its own global connection settings:

| Flag | Environment variable | Default |
| --- | --- | --- |
| `--endpoint` | `RS3_ENDPOINT` | `http://localhost:9000` |
| `--access-key` | `RS3_ACCESS_KEY` | `minioadmin` |
| `--secret-key` | `RS3_SECRET_KEY` | `minioadmin` |
| `--region` | `RS3_REGION` | `us-east-1` |

```bash
RS3_ENDPOINT=http://localhost:9000 \
RS3_ACCESS_KEY=local-access \
RS3_SECRET_KEY=local-secret \
cargo run --release --bin rs3-cli -- ls
```

The server accepts any signing region in a valid SigV4 credential scope. The
client defaults to `us-east-1`.

## Runtime Limits

The following limits are compile-time constants in `src/server.rs`:

| Limit | Value |
| --- | --- |
| Request body | 5 GiB |
| Object key | 1024 bytes |
| Bucket name | 3 to 63 characters |

Requests and object responses are currently buffered in memory. Set upstream
proxy limits and concurrency accordingly.

## Multiple Users and Instances

One server process supports one credential pair. It does not implement users,
roles, bucket policies, or ACLs.

Do not run multiple `rs3` processes against the same data directory. There is
no cross-process coordination for bucket deletion, multipart state, or writes
to the same key.

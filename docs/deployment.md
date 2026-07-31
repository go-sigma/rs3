# Deployment

`rs3` can run as a native binary or in the included container image. Use one
server process per data directory.

## Release Build

```bash
cargo build --locked --release --bins
```

This creates:

- `target/release/rs3`
- `target/release/rs3-cli`

The binaries use the host target. Install the appropriate Rust target and
linker when cross-compiling.

## Native Binary

Create an unprivileged account and data directory:

```bash
rs3_binary=target/release/rs3
sudo useradd --system --home /var/lib/rs3 --shell /usr/sbin/nologin rs3
sudo install -d -o rs3 -g rs3 /var/lib/rs3/data
sudo install -d -o root -g rs3 -m 0750 /etc/rs3
sudo install -m 0755 "$rs3_binary" /usr/local/bin/rs3
```

Store credentials in a root-readable environment file:

```ini
# /etc/rs3/rs3.env
RS3_HOST=127.0.0.1
RS3_PORT=9000
RS3_DATA_DIR=/var/lib/rs3/data
RS3_ACCESS_KEY=replace-this-access-key
RS3_SECRET_KEY=replace-this-secret-key
```

Protect the file and run the server:

```bash
sudo chown root:rs3 /etc/rs3/rs3.env
sudo chmod 0640 /etc/rs3/rs3.env
sudo -u rs3 sh -c 'set -a; . /etc/rs3/rs3.env; exec /usr/local/bin/rs3'
```

## Docker

The included `Dockerfile` builds both binaries and creates a Debian runtime
image with CA certificates and `curl` for its health check. The process runs
as UID and GID `10001`.

```bash
docker build --tag rs3:local .
docker run --detach \
  --name rs3 \
  --restart unless-stopped \
  --publish 127.0.0.1:9000:9000 \
  --env RS3_ACCESS_KEY=replace-this-access-key \
  --env RS3_SECRET_KEY=replace-this-secret-key \
  --volume rs3-data:/var/lib/rs3/data \
  rs3:local
```

Pass credentials through your container platform's secret mechanism rather
than committing them to an image or Compose file.

Run the bundled client in the image:

```bash
docker run --rm \
  --network container:rs3 \
  --entrypoint /usr/local/bin/rs3-cli \
  --env RS3_ENDPOINT=http://localhost:9000 \
  --env RS3_ACCESS_KEY=replace-this-access-key \
  --env RS3_SECRET_KEY=replace-this-secret-key \
  rs3:local ls
```

## Availability Check

The unauthenticated health probes are intended for container and process
supervision:

```bash
curl --fail http://localhost:9000/healthz
curl --fail http://localhost:9000/readyz
```

`/healthz` checks that the HTTP server is responding. `/readyz` also checks
that the configured data directory exists, is a directory, and is readable.
The Docker image uses `/readyz` for its `HEALTHCHECK`.

## Backup and Restore

Data consists of ordinary files under `RS3_DATA_DIR`. For a consistent backup,
stop writes and copy the entire directory, including `.uploads` if in-progress
multipart uploads must survive:

```bash
sudo tar -C /var/lib/rs3 -czf /srv/backups/rs3-data.tar.gz data
```

Restore into an empty data directory with ownership matching the service user.
Validate the restored buckets with `rs3-cli ls`.

## Logging

Logs are written to stderr through `tracing`. Set `RUST_LOG` to control
verbosity:

```bash
RUST_LOG=debug target/release/rs3
```

## Operational Constraints

- A data directory must be owned by exactly one server process
- Large request and response bodies are buffered in memory
- There is no replication, erasure coding, versioning, or durability journal
- Authentication uses one credential pair for the entire server
- Request rate limiting must be provided externally

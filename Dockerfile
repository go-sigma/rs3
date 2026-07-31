FROM rust:1.97-slim-trixie AS builder

WORKDIR /build

RUN apt-get update \
    && apt-get install --yes --no-install-recommends gcc libc6-dev \
    && apt-get clean \
    && find /var/lib/apt/lists -type f -delete

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --locked --release --bins

FROM debian:trixie-slim AS runtime

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && apt-get clean \
    && find /var/lib/apt/lists -type f -delete \
    && groupadd --gid 10001 rs3 \
    && useradd --uid 10001 --gid rs3 --no-create-home --shell /usr/sbin/nologin rs3 \
    && mkdir -p /var/lib/rs3/data \
    && chown -R rs3:rs3 /var/lib/rs3

COPY --from=builder /build/target/release/rs3 /usr/local/bin/rs3
COPY --from=builder /build/target/release/rs3-cli /usr/local/bin/rs3-cli

ENV RS3_HOST=0.0.0.0 \
    RS3_PORT=9000 \
    RS3_DATA_DIR=/var/lib/rs3/data

USER 10001:10001
WORKDIR /var/lib/rs3

VOLUME ["/var/lib/rs3/data"]
EXPOSE 9000

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl --fail "http://127.0.0.1:${RS3_PORT}/readyz" || exit 1

ENTRYPOINT ["/usr/local/bin/rs3"]

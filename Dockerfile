# syntax=docker/dockerfile:1.10
FROM rust:1.98-bookworm AS build

WORKDIR /workspace
ARG TARGETARCH
ARG SCCACHE_VERSION=0.17.0
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && case "$TARGETARCH" in \
         amd64) target=x86_64-unknown-linux-musl; sha256=67c4a96dd237c1f518f6b36083f270f9976d516f1e57fce891755ea782e50006 ;; \
         arm64) target=aarch64-unknown-linux-musl; sha256=821a86343191aa1cbab74bd42f9e93c9a63bf85e4742945f40d3ae84193c1c77 ;; \
         *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
       esac \
    && archive="sccache-v${SCCACHE_VERSION}-${target}.tar.gz" \
    && curl -fsSLo "/tmp/$archive" \
       "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/$archive" \
    && echo "$sha256  /tmp/$archive" | sha256sum -c - \
    && tar -xzf "/tmp/$archive" --strip-components=1 -C /usr/local/bin \
       "sccache-v${SCCACHE_VERSION}-${target}/sccache" \
    && rm -f "/tmp/$archive" \
    && rm -rf /var/lib/apt/lists/*
ARG RUSTC_WRAPPER
ARG SCCACHE_BUCKET
ARG SCCACHE_ENDPOINT
ARG SCCACHE_REGION=auto
ARG SCCACHE_PREFIX=rust-v1
COPY mb-printer-sdk ./mb-printer-sdk
COPY mb-printer-cli ./mb-printer-cli
RUN --mount=type=secret,id=aws_access_key_id,env=AWS_ACCESS_KEY_ID \
    --mount=type=secret,id=aws_secret_access_key,env=AWS_SECRET_ACCESS_KEY \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/workspace/mb-printer-cli/target \
    export RUSTC_WRAPPER SCCACHE_BUCKET SCCACHE_ENDPOINT SCCACHE_REGION \
           SCCACHE_S3_USE_SSL=true SCCACHE_S3_KEY_PREFIX="$SCCACHE_PREFIX" \
           SCCACHE_BASEDIRS=/workspace:/usr/local/cargo/registry \
    && cargo build --manifest-path mb-printer-cli/Cargo.toml --release --locked \
    && cp mb-printer-cli/target/release/mb-printer /tmp/mb-printer

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 10001 --shell /usr/sbin/nologin mbprint \
    && install -d -o mbprint -g mbprint /var/lib/mb-printer
COPY --from=build /tmp/mb-printer /usr/local/bin/mb-printer
USER mbprint
VOLUME ["/var/lib/mb-printer"]
ENTRYPOINT ["mb-printer"]

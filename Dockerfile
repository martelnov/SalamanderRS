# SalamanderRS Web: test container; build the web binary from the Rust workspace.
FROM rust:1-bookworm AS builder

# salamander-core links these system libraries by their versioned SONAME.
RUN apt-get update \
 && apt-get install -y --no-install-recommends libarchive13 libsodium23 \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .
RUN cargo build --release --locked -p salamander-web

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates libarchive13 libsodium23 \
 && rm -rf /var/lib/apt/lists/* \
 && mkdir -p /data /models

COPY --from=builder /src/target/release/salamander-rs-web /usr/local/bin/salamander-rs-web

ENV SALAMANDER_DATA_ROOT=/data \
    SALAMANDER_MODELS_ROOT=/models \
    SALAMANDER_ENABLE_WRITES=1 \
    SALAMANDER_PORT=8090

# AI Platform will override this with the user's assigned numeric UID:GID.
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/salamander-rs-web"]

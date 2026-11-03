# WordCraft Collaborative server image. Build from the repository root (deploy/compose.yaml does):
#   docker build -f apps/wordcraft-collab-server/Dockerfile -t wordcraft-collab:local .
# The whole workspace is the build context because Cargo loads every member's manifest; only the
# server and its dependencies are compiled.

ARG RUST_VERSION=1.99

FROM rust:${RUST_VERSION}-trixie AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p wordcraft-collab-server \
    && install -D target/release/wordcraft-collab-server /out/wordcraft-collab-server

FROM debian:trixie-slim
# ca-certificates: the OIDC issuer's keys are fetched over HTTPS.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 wordcraft \
    && useradd --system --uid 10001 --gid wordcraft --home-dir /data --no-create-home wordcraft \
    && install -d -o wordcraft -g wordcraft /data
COPY --from=build /out/wordcraft-collab-server /app/wordcraft-collab-server

USER wordcraft
ENV LISTEN_ADDR=0.0.0.0:3000 \
    DATABASE_URL=sqlite:///data/wordcraft.db \
    DOCUMENT_STORAGE=/data/documents \
    RUST_LOG=info
VOLUME ["/data"]
EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=5s --retries=3 CMD ["/app/wordcraft-collab-server", "healthcheck"]
ENTRYPOINT ["/app/wordcraft-collab-server"]
CMD ["serve"]

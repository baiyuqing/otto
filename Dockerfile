# syntax=docker/dockerfile:1
FROM rust:1.99.1-bookworm AS build
WORKDIR /src
COPY rust-toolchain.toml ./
# Optional CA secret for builds behind an HTTPS inspection proxy. Nothing
# session-specific is stored in an image layer; ordinary Docker Desktop builds
# use the image's normal trust roots.
COPY . .
ARG OTTO_GIT_HASH=unknown
ARG TARGETARCH
RUN --mount=type=secret,id=proxy_ca \
    --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,id=otto-target-${TARGETARCH},target=/src/target \
    if [ -f /run/secrets/proxy_ca ]; then \
      export SSL_CERT_FILE=/run/secrets/proxy_ca CARGO_HTTP_CAINFO=/run/secrets/proxy_ca; \
    fi; \
    cargo fetch --locked \
    && cargo build --offline --locked --release -p otto \
    && install -D target/release/otto /out/otto

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates bash git socat python3 \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 1000 otto \
    && useradd --uid 1000 --gid otto --create-home --shell /bin/bash otto \
    && install -d -o otto -g otto -m 0700 /home/otto/.otto \
    && install -d -o otto -g otto /workspace
COPY --from=build /out/otto /usr/local/bin/otto
COPY --chown=otto:otto .otto/skills/sandbox-setup /home/otto/.otto/skills/sandbox-setup
ENV HOME=/home/otto
USER otto
WORKDIR /workspace
ENTRYPOINT ["otto"]
CMD ["--help"]

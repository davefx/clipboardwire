FROM rust:1-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
RUN apt-get update && apt-get install -y \
    libgtk-3-dev \
    libayatana-appindicator3-dev \
    libxdo-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release -p clipboardwire

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y \
    libgtk-3-0 \
    libayatana-appindicator3-1 \
    libxdo3 \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN groupadd -g 1001 cw && useradd -u 1001 -g cw -M -s /bin/sh cw
COPY --from=builder --chown=cw:cw /app/target/release/clipboardwire /usr/local/bin/clipboardwire
USER cw
EXPOSE 8484
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD bash -c '</dev/tcp/localhost/8484'
CMD ["clipboardwire", "serve"]

# ---- build stage ----
FROM rust:1-bookworm AS builder
WORKDIR /build

# Cache dependencies first.
COPY Cargo.toml ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release 2>/dev/null || true
RUN rm -rf src

# Real sources. `templates/` is needed at compile time (Askama embeds them).
COPY migrations ./migrations
COPY templates ./templates
COPY src ./src
RUN touch src/main.rs && cargo build --release

# ---- runtime stage ----
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        ffmpeg ca-certificates fontconfig fonts-dejavu-core \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /build/target/release/infistreamer /usr/local/bin/infistreamer
COPY static ./static

ENV DATA_DIR=/app/data \
    BIND_ADDR=0.0.0.0:8080
EXPOSE 8080
VOLUME ["/app/data"]

CMD ["infistreamer"]

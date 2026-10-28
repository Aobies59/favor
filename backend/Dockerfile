# ── Build ────────────────────────────────────────────────────────────────
FROM rust:1-slim-bookworm AS build
WORKDIR /src

# Cache dependencies separately from the app code.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src target/release/deps/favor-* target/release/favor*

COPY migrations ./migrations
COPY src ./src
RUN cargo build --release --locked

# ── Run ──────────────────────────────────────────────────────────────────
FROM debian:bookworm-slim
RUN useradd --system --uid 10001 favor
WORKDIR /app
COPY --from=build /src/target/release/favor /app/favor
COPY static /app/static

USER favor
ENV FAVOR_PORT=8080 FAVOR_STATIC_DIR=/app/static
EXPOSE 8080
CMD ["/app/favor"]

FROM rust:1.83-slim AS builder
WORKDIR /app
COPY Cargo.toml Cargo.toml
COPY src/ src/
RUN cargo build --release

FROM gcr.io/distroless/cc-debian12
COPY --from=builder /app/target/release/ghpool /ghpool
EXPOSE 8080
ENTRYPOINT ["/ghpool"]

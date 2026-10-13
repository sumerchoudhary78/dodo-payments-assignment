# One image with all three binaries; docker-compose picks the binary per service.
FROM rust:1.96-slim AS build
WORKDIR /app
COPY . .
# Cache mounts keep the cargo registry and build artifacts between builds.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --workspace \
 && mkdir -p /out \
 && cp target/release/invoice-service target/release/mock-psp target/release/webhook-receiver /out/

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --no-create-home app
COPY --from=build /out/ /usr/local/bin/
USER app

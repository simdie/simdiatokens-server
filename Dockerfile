FROM rust:1.90-slim AS builder
RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates sqlite3 && rm -rf /var/lib/apt/lists/*

# Create the data directory and set permissions
RUN mkdir -p /data && chmod 777 /data

COPY --from=builder /app/target/release/simdiatokens_server /usr/local/bin/
EXPOSE 8080
CMD ["simdiatokens_server"]
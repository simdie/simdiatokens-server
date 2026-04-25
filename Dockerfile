FROM rust:1.87-slim AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates sqlite3 && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/simdiatokens_server /usr/local/bin/
EXPOSE 8080
CMD ["simdiatokens_server"]
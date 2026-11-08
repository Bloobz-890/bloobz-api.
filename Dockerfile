FROM rust:1-slim-bookworm AS build
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/ani-api /usr/local/bin/ani-api
ENV RUST_LOG=info
CMD ["ani-api"]

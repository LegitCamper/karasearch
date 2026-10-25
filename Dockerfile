FROM rust:slim-bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/karasearch /usr/local/bin/karasearch
USER 10001:10001
ENV BIND_ADDR=0.0.0.0:8787
EXPOSE 8787
ENTRYPOINT ["karasearch"]

FROM rust:1-slim AS build
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
WORKDIR /app
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/llm-router /usr/local/bin/llm-router
COPY config.example.toml /app/config.toml
EXPOSE 8080
CMD ["llm-router"]

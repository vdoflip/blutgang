FROM rust:latest AS build

WORKDIR /app

COPY . /app

RUN apt-get update && apt-get install -y libssl-dev
RUN cargo build --release


FROM debian:bookworm

RUN mkdir /app
RUN apt-get update && apt-get install -y openssl ca-certificates
COPY --from=build /app/target/release/blutgang /app/blutgang
COPY --from=build /app/example_config.toml /app/config.toml
WORKDIR /app
CMD ["./blutgang", "-c", "config.toml"]

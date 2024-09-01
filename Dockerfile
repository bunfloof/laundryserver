FROM rust:1.80 AS builder
WORKDIR /usr/src/laundryserver
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get -o Acquire::Check-Valid-Until=false -o Acquire::Check-Date=false update && \
    apt-get install -y libssl-dev && \
    rm -rf /var/lib/apt/lists/*
COPY --from=builder /usr/src/laundryserver/target/release/laundryserver /usr/local/bin/laundryserver
EXPOSE 25651 25652
CMD ["laundryserver"]

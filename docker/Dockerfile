FROM rust:trixie AS builder

WORKDIR /app

RUN git clone https://github.com/nappa85/p0sticcio_bot.git && cd p0sticcio_bot && cargo build --release

FROM debian:trixie-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/p0sticcio_bot/target/release/p0sticcio_bot /usr/local/bin/p0sticcio_bot
COPY config.yaml /usr/local/bin/

RUN chmod +x /usr/local/bin/p0sticcio_bot

WORKDIR /usr/local/bin/

CMD ["/usr/local/bin/p0sticcio_bot"]

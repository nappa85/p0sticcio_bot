FROM debian:11-slim

RUN apt-get update && apt-get install -y libssl1.1 ca-certificates && rm -rf /var/lib/apt/lists/*

COPY p0sticcio_bot /usr/local/bin/

RUN chmod +x /usr/local/bin/p0sticcio_bot

CMD p0sticcio_bot

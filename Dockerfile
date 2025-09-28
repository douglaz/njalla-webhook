# Use pre-built binary
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates && \
    rm -rf /var/lib/apt/lists/*

COPY target/release/njalla-webhook /usr/local/bin/njalla-webhook
RUN chmod +x /usr/local/bin/njalla-webhook

ENV WEBHOOK_HOST=0.0.0.0
ENV WEBHOOK_PORT=8888

EXPOSE 8888

CMD ["njalla-webhook"]
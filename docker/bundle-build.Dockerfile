FROM debian:bookworm-slim

LABEL org.opencontainers.image.source=https://github.com/phoxal/phoxal-cli

COPY bundle/ /seed/

CMD ["sh", "-c", "mkdir -p /workspace && find /workspace -mindepth 1 -maxdepth 1 -exec rm -rf {} + && cp -a /seed/. /workspace/ && exec tail -f /dev/null"]

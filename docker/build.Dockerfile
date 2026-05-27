FROM debian:bookworm-slim

ARG BINARY_NAME
ARG BINARY_SOURCE

LABEL org.opencontainers.image.source=https://github.com/jbernavaprah/robot-framework
LABEL org.opencontainers.image.description="${BINARY_NAME}"
LABEL org.opencontainers.image.licenses=MIT

ENV BINARY_NAME=${BINARY_NAME}

WORKDIR /app

# Assumes the host environment has built and staged the binary for image assembly.
COPY ${BINARY_SOURCE} /app/${BINARY_NAME}

CMD ["sh", "-c", "/app/${BINARY_NAME}"]

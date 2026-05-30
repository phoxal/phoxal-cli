FROM alpine:3.20

LABEL org.opencontainers.image.source=https://github.com/phoxal/phoxal-cli

RUN apk add --no-cache curl ca-certificates

RUN set -eu && \
    mkdir -p /extra-firmware && \
    base="https://gitlab.com/kernel-firmware/linux-firmware/-/raw/main" && \
    files="$( \
      curl -fsSL "$base/WHENCE" | \
      grep -Eo 'intel/iwlwifi/iwlwifi-cc-a0-[0-9]+\.ucode' | \
      sort -u \
    )" && \
    test -n "$files" && \
    for relative_path in $files; do \
      file_name="$(basename "$relative_path")" && \
      curl -fsSL "$base/$relative_path" -o "/extra-firmware/$file_name"; \
    done

CMD ["sh", "-c", "sleep infinity"]

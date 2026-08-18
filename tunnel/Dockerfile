# Tunnel server image: the pinned frp release on a plain Alpine base.
# Built locally by compose — no third-party frp image to trust; the
# checksums below come from the release's own frp_sha256_checksums.txt.
FROM alpine:3.22

ARG TARGETARCH
ENV FRP_VERSION=0.61.1

RUN set -e; \
    case "$TARGETARCH" in \
      amd64) sha=bff260b68ca7b1461182a46c4f34e9709ba32764eed30a15dd94ac97f50a2c40 ;; \
      arm64) sha=af6366f2b43920ebfe6235dba6060770399ed1fb18601e5818552bd46a7621f8 ;; \
      *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    wget -qO /tmp/frp.tar.gz "https://github.com/fatedier/frp/releases/download/v${FRP_VERSION}/frp_${FRP_VERSION}_linux_${TARGETARCH}.tar.gz"; \
    echo "$sha  /tmp/frp.tar.gz" | sha256sum -c -; \
    tar -xzf /tmp/frp.tar.gz -C /tmp; \
    mv "/tmp/frp_${FRP_VERSION}_linux_${TARGETARCH}/frps" /usr/local/bin/frps; \
    rm -rf /tmp/frp*

ENTRYPOINT ["frps", "-c", "/etc/frp/frps.toml"]

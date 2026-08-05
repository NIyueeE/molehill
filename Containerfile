# Packages pre-built static musl binaries into a scratch image.
# Build context layout:
#   bin/<arch>/molehill            (arch = amd64, arm64, ...)
#   bin/<arch>/ca-certificates.crt
FROM scratch

WORKDIR /app
ARG TARGETARCH
COPY bin/${TARGETARCH}/molehill /app/molehill
COPY bin/${TARGETARCH}/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
USER 1000:1000
ENTRYPOINT ["./molehill"]

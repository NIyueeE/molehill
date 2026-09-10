# Packages pre-built static musl binaries into a scratch image.
# Build context layout:
#   bin/<arch>/molehill            (arch = amd64, arm64, ...)
FROM scratch

WORKDIR /app
ARG TARGETARCH
COPY bin/${TARGETARCH}/molehill /app/molehill
USER 1000:1000
ENTRYPOINT ["./molehill"]

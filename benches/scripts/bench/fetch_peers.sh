#!/usr/bin/env bash
# Fetch pinned peer tools for the benchmark matrix into $PEER_DIR.
# Versions are pinned deliberately — bump them here and in README when needed.
set -euo pipefail

PEER_DIR=${PEER_DIR:-/tmp/bench-peers}
mkdir -p "$PEER_DIR"
ARCH=$(uname -m)
case "$ARCH" in
  x86_64)  FRP_ARCH=amd64; BORE_ARCH=x86_64;  CHISEL_ARCH=amd64 ;;
  aarch64) FRP_ARCH=arm64; BORE_ARCH=aarch64; CHISEL_ARCH=arm64 ;;
  *) echo "unsupported arch: $ARCH" >&2; exit 1 ;;
esac

fetch() { # url out
  [ -s "$2" ] && { echo "cached: $2"; return 0; }
  curl -fsSL --max-time 180 -o "$2" "$1"
}

## frp -----------------------------------------------------------------------
FRP_VER=0.71.0
FRP_DIR="$PEER_DIR/frp_${FRP_VER}_linux_${FRP_ARCH}"
if [ -x "$FRP_DIR/frps" ]; then
  echo "frp: cached"
else
  echo "frp: downloading v$FRP_VER"
  fetch "https://github.com/fatedier/frp/releases/download/v$FRP_VER/frp_${FRP_VER}_linux_${FRP_ARCH}.tar.gz" "$PEER_DIR/frp.tar.gz"
  tar -xzf "$PEER_DIR/frp.tar.gz" -C "$PEER_DIR"
  rm -f "$PEER_DIR/frp.tar.gz"
fi
# stable path for consumers (run_bench.sh): $PEER_DIR/frp/frps
ln -sfn "frp_${FRP_VER}_linux_${FRP_ARCH}" "$PEER_DIR/frp"

## bore ----------------------------------------------------------------------
BORE_VER=0.6.0
if [ -x "$PEER_DIR/bore" ]; then
  echo "bore: cached"
elif fetch "https://github.com/ekzhang/bore/releases/download/v$BORE_VER/bore-v$BORE_VER-${BORE_ARCH}-unknown-linux-musl.tar.gz" "$PEER_DIR/bore.tar.gz"; then
  tar -xzf "$PEER_DIR/bore.tar.gz" -C "$PEER_DIR"
  rm -f "$PEER_DIR/bore.tar.gz"
  # the tarball may hold the binary at root or inside a versioned directory
  [ -x "$PEER_DIR/bore" ] || mv "$PEER_DIR"/bore*/bore "$PEER_DIR/bore"
  chmod +x "$PEER_DIR/bore"
else
  echo "bore: release asset unavailable, falling back to cargo install"
  cargo install bore-cli --version "$BORE_VER" --root "$PEER_DIR/cargo"
  cp "$PEER_DIR/cargo/bin/bore" "$PEER_DIR/bore"
fi

## chisel --------------------------------------------------------------------
CHISEL_VER=1.10.1
if [ -x "$PEER_DIR/chisel" ]; then
  echo "chisel: cached"
else
  echo "chisel: downloading v$CHISEL_VER"
  fetch "https://github.com/jpillora/chisel/releases/download/v$CHISEL_VER/chisel_${CHISEL_VER}_linux_${CHISEL_ARCH}.gz" "$PEER_DIR/chisel.gz"
  gunzip -f "$PEER_DIR/chisel.gz"
  chmod +x "$PEER_DIR/chisel"
fi

## rathole (upstream) --------------------------------------------------------
RATHOLE_TAG=v0.5.0
if [ -x "$PEER_DIR/rathole" ]; then
  echo "rathole: cached"
else
  echo "rathole: downloading $RATHOLE_TAG prebuilt release binary"
  asset=""
  for t in gnu musl; do
    u="https://github.com/rathole-org/rathole/releases/download/$RATHOLE_TAG/rathole-${BORE_ARCH}-unknown-linux-$t.zip"
    if curl -fL --max-time 180 -o "$PEER_DIR/rathole.zip" "$u" 2>/dev/null; then asset=$t; break; fi
    rm -f "$PEER_DIR/rathole.zip"
  done
  if [ -n "$asset" ]; then
    unzip -oq "$PEER_DIR/rathole.zip" -d "$PEER_DIR/rathole-zip"
    mv "$PEER_DIR"/rathole-zip/rathole "$PEER_DIR/rathole" 2>/dev/null \
      || mv "$PEER_DIR"/rathole-zip/*/rathole "$PEER_DIR/rathole"
    chmod +x "$PEER_DIR/rathole"
    rm -rf "$PEER_DIR/rathole.zip" "$PEER_DIR/rathole-zip"
  else
    echo "rathole: release asset unavailable, building from source (a few minutes)"
    if [ ! -d "$PEER_DIR/rathole-src" ]; then
      git clone --depth 1 --branch "$RATHOLE_TAG" https://github.com/rathole-org/rathole "$PEER_DIR/rathole-src"
    fi
    # The v0.5.0 lockfile pins `time 0.3.29`, which no longer compiles on
    # current rustc (E0282); bump it within semver before building.
    (cd "$PEER_DIR/rathole-src" \
        && { cargo update -p time 2>/dev/null || true; } \
        && cargo build --release)
    cp "$PEER_DIR/rathole-src/target/release/rathole" "$PEER_DIR/rathole"
  fi
fi

echo "== peer versions =="
"$PEER_DIR/frp/frps" --version | head -1
"$PEER_DIR/bore" --version | head -1
"$PEER_DIR/chisel" --version | head -1
"$PEER_DIR/rathole" --version | head -1
echo "peers ready in $PEER_DIR"

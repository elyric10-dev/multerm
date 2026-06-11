#!/usr/bin/env bash
# Package Linux release binaries into a tarball.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

VERSION="${1:-dev}"
ARCH="$(uname -m)"
DIR="dist/multerm-${VERSION}-linux-${ARCH}"
BIN_UI="${ROOT}/target/release/multerm-ui"
BIN_TERM="${ROOT}/target/release/multerm"

if [[ ! -x "$BIN_UI" ]]; then
  echo "error: missing release binary — run: cargo build --release -p multerm-app --bin multerm-ui" >&2
  exit 1
fi

rm -rf "$DIR"
mkdir -p "$DIR/bin"

cp "$BIN_UI" "$DIR/bin/multerm-ui"
if [[ -x "$BIN_TERM" ]]; then
  cp "$BIN_TERM" "$DIR/bin/multerm"
fi
chmod +x "$DIR/bin/"*

cat > "$DIR/README.txt" <<TXT
Multerm ${VERSION} (linux-${ARCH})

Run the workspace UI:
  ./bin/multerm-ui

Optional lean GPU terminal:
  ./bin/multerm

Requires a GPU with Vulkan or compatible wgpu backend.
TXT

TARBALL="dist/multerm-${VERSION}-linux-${ARCH}.tar.gz"
tar -C dist -czf "$TARBALL" "$(basename "$DIR")"
echo "Created ${TARBALL}"

#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
IMG="${TOKIMO_IMG_DIR:-$ROOT/img}"
DIST="$ROOT/dist"
mkdir -p "$DIST"
SHA=$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null || echo "local")
OUT="$DIST/tokimo-image-$SHA.tar.zst"
tar --zstd -cf "$OUT" -C "$IMG" .
echo ">>> wrote $OUT"

#!/usr/bin/env bash
set -euo pipefail
IMG="${TOKIMO_IMG_DIR:-$(cd "$(dirname "$0")/../.." && pwd)/img}"
cd "$IMG"
sha256sum -c SHA256SUMS

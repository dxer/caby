#!/usr/bin/env bash
# Build a fully static, self-contained `caby` binary (musl) — zero runtime
# dependencies: no Node, no Python, no glibc, no shared libraries.
#
# Requirements: rustup + musl-gcc (apt install musl-tools on Debian/Ubuntu).
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET="${TARGET:-x86_64-unknown-linux-musl}"
export PATH="$HOME/.cargo/bin:$PATH"

rustup target add "$TARGET" >/dev/null 2>&1 || true

# ring (TLS via ureq) needs a C compiler targeting musl
export CC_${TARGET//-/_}="${CC_${TARGET//-/_}:-musl-gcc}"

cargo build --release --target "$TARGET"

OUT="target/$TARGET/release/caby"
echo
echo "built: $OUT ($(du -h "$OUT" | cut -f1))"
"$OUT" version
file "$OUT"
if command -v ldd >/dev/null; then
  ldd "$OUT" 2>&1 || true
fi
echo "(ldd printing 'statically linked' / 'not a dynamic executable' confirms zero libc deps)"
#!/usr/bin/env bash

set -euo pipefail

# SYSCOIN: The verifier ROM intentionally permits rust-lld to report an
# arbitrary LOADADDR for an empty `.data` section. Prove that the independent
# nonempty-data assertion still rejects initialized writable state.
readonly VERIFIER_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly HOST_TRIPLE="$(rustc -vV | awk '/^host:/ { print $2 }')"
readonly RUST_LLD="$(rustc --print sysroot)/lib/rustlib/$HOST_TRIPLE/bin/rust-lld"
readonly TEST_DIR="$(mktemp -d "${TMPDIR:-/tmp}/airbender-linker-data.XXXXXX")"

cleanup() {
    rm -rf -- "$TEST_DIR"
}
trap cleanup EXIT

[[ -x "$RUST_LLD" ]] || {
    echo "rust-lld is unavailable: $RUST_LLD" >&2
    exit 1
}

# rust-lld's binary input format maps these bytes to an initialized `.data`
# input section without requiring a host assembler or a prebuilt object.
printf '\001\000\000\000' > "$TEST_DIR/nonempty-data.bin"

set +e
"$RUST_LLD" \
    -flavor gnu \
    -m elf32lriscv \
    -b binary \
    -T "$VERIFIER_DIR/src/lds/memory.x" \
    -T "$VERIFIER_DIR/src/lds/link.x" \
    "$TEST_DIR/nonempty-data.bin" \
    -o "$TEST_DIR/nonempty-data.elf" \
    > "$TEST_DIR/link.log" 2>&1
readonly LINK_STATUS=$?
set -e

if ((LINK_STATUS == 0)); then
    echo "linker unexpectedly accepted nonempty initialized .data" >&2
    exit 1
fi
if ! grep -Fq ".data section detected in the input files" "$TEST_DIR/link.log"; then
    echo "linker failed for an unexpected reason:" >&2
    sed -n '1,120p' "$TEST_DIR/link.log" >&2
    exit 1
fi

echo "nonempty .data linker rejection passed"

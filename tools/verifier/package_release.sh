#!/usr/bin/env bash

set -euo pipefail

readonly VERIFIER_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly PINNED_MANIFEST="$VERIFIER_DIR/../../execution_utils/verifier-artifacts.sha256"
readonly SOURCE_DATE_EPOCH=946684800
readonly -a ARTIFACTS=(
    base_layer.bin
    recursion_layer.bin
    recursion_log_23_layer.bin
    recursion_layer_no_delegation.bin
    final_recursion_layer.bin
    base_layer_with_output.bin
    recursion_layer_with_output.bin
    recursion_log_23_layer_with_output.bin
    recursion_layer_no_delegation_with_output.bin
    final_recursion_layer_with_output.bin
    universal.bin
    universal_no_delegation.bin
    universal.reduced.vk.json
    universal.reduced_log23.vk.json
    universal_no_delegation.final.vk.json
    base_layer.reduced.vk.json
    recursion_layer.reduced.vk.json
    recursion_log_23_layer.reduced.vk.json
    recursion_layer_no_delegation.final.vk.json
    final_recursion_layer.final.vk.json
    build-profiles.tsv
    build-environment.txt
    artifact-manifest.sha256
    artifact-manifest.tsv
)

[[ $# -eq 1 && "$1" == /* ]] || {
    echo "Usage: package_release.sh /absolute/path/to/verifier-artifacts.tar.gz" >&2
    exit 2
}
readonly OUTPUT=$1
readonly CHECKSUM_OUTPUT="$OUTPUT.sha256"
[[ ! -e "$OUTPUT" && ! -L "$OUTPUT" ]] || {
    echo "release archive already exists: $OUTPUT" >&2
    exit 1
}
[[ ! -e "$CHECKSUM_OUTPUT" && ! -L "$CHECKSUM_OUTPUT" ]] || {
    echo "release checksum already exists: $CHECKSUM_OUTPUT" >&2
    exit 1
}
readonly OUTPUT_PARENT=$(dirname -- "$OUTPUT")
[[ -d "$OUTPUT_PARENT" && ! -L "$OUTPUT_PARENT" ]] || {
    echo "release output parent must be an existing non-symlink directory" >&2
    exit 1
}
tar --version | grep -q 'GNU tar' || {
    echo "GNU tar is required for deterministic release packaging" >&2
    exit 1
}

for artifact in "${ARTIFACTS[@]}"; do
    [[ -f "$VERIFIER_DIR/$artifact" && ! -L "$VERIFIER_DIR/$artifact" ]] || {
        echo "missing regular verifier release artifact: $artifact" >&2
        exit 1
    }
done

manifest_records=$(wc -l < "$VERIFIER_DIR/artifact-manifest.sha256")
manifest_records=${manifest_records//[[:space:]]/}
[[ "$manifest_records" -eq 22 ]] || {
    echo "canonical Docker reproduction manifest must contain exactly 22 records" >&2
    exit 1
}
cmp "$PINNED_MANIFEST" "$VERIFIER_DIR/artifact-manifest.sha256" >/dev/null || {
    echo "reproduced artifacts differ from the source-pinned verifier artifact table" >&2
    exit 1
}

(
    cd "$VERIFIER_DIR"
    while read -r expected_hash artifact extra; do
        [[ -z "${extra:-}" && -f "$artifact" && ! -L "$artifact" ]] || exit 1
        printf '%s  %s\n' "$(sha256sum "$artifact" | awk '{print $1}')" "$artifact"
    done < artifact-manifest.sha256
) | cmp - "$VERIFIER_DIR/artifact-manifest.sha256" >/dev/null || {
    echo "verifier artifact manifest verification failed" >&2
    exit 1
}

ARCHIVE_TMP=$(mktemp "$OUTPUT_PARENT/.verifier-artifacts.XXXXXX")
CHECKSUM_TMP=$(mktemp "$OUTPUT_PARENT/.verifier-artifacts-sha256.XXXXXX")
cleanup() {
    local exit_code=$?
    trap - EXIT INT TERM
    rm -f -- "$ARCHIVE_TMP" "$CHECKSUM_TMP"
    exit "$exit_code"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

(
    cd "$VERIFIER_DIR"
    tar --sort=name \
        --format=ustar \
        --mtime="@$SOURCE_DATE_EPOCH" \
        --owner=0 \
        --group=0 \
        --numeric-owner \
        --mode='u=rw,go=r' \
        -cf - \
        "${ARTIFACTS[@]}" | gzip -9 -n > "$ARCHIVE_TMP"
)
archive_hash=$(sha256sum "$ARCHIVE_TMP" | awk '{print $1}')
printf '%s  %s\n' "$archive_hash" "$(basename -- "$OUTPUT")" > "$CHECKSUM_TMP"
mv -- "$ARCHIVE_TMP" "$OUTPUT"
mv -- "$CHECKSUM_TMP" "$CHECKSUM_OUTPUT"
printf 'release archive SHA-256: %s\n' "$archive_hash"
printf 'artifact manifest SHA-256: %s\n' \
    "$(sha256sum "$VERIFIER_DIR/artifact-manifest.sha256" | awk '{print $1}')"

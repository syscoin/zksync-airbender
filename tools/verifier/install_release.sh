#!/usr/bin/env bash

set -euo pipefail

readonly VERIFIER_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly PINNED_MANIFEST="$VERIFIER_DIR/../../execution_utils/verifier-artifacts.sha256"

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

usage() {
    cat >&2 <<'EOF'
Usage: install_release.sh ARCHIVE_OR_HTTPS_URL ARCHIVE_SHA256 MANIFEST_SHA256 ABSOLUTE_DESTINATION

Installs one exact verifier release bundle into a new directory. The caller
must pin both the outer archive and its internal artifact manifest.
EOF
    exit 2
}

[[ $# -eq 4 ]] || usage
readonly SOURCE=$1
readonly EXPECTED_ARCHIVE_SHA256=$2
readonly EXPECTED_MANIFEST_SHA256=$3
readonly REQUESTED_DESTINATION=$4

is_sha256() {
    [[ "$1" =~ ^[0-9a-f]{64}$ ]]
}

is_sha256 "$EXPECTED_ARCHIVE_SHA256" || {
    echo "archive SHA-256 must be 64 lowercase hexadecimal characters" >&2
    exit 1
}
is_sha256 "$EXPECTED_MANIFEST_SHA256" || {
    echo "manifest SHA-256 must be 64 lowercase hexadecimal characters" >&2
    exit 1
}
[[ "$REQUESTED_DESTINATION" == /* ]] || {
    echo "destination must be absolute" >&2
    exit 1
}
[[ ! -e "$REQUESTED_DESTINATION" && ! -L "$REQUESTED_DESTINATION" ]] || {
    echo "destination already exists; refusing replacement: $REQUESTED_DESTINATION" >&2
    exit 1
}

readonly DESTINATION_PARENT=$(dirname -- "$REQUESTED_DESTINATION")
readonly DESTINATION_NAME=$(basename -- "$REQUESTED_DESTINATION")
[[ -d "$DESTINATION_PARENT" && ! -L "$DESTINATION_PARENT" ]] || {
    echo "destination parent must be an existing non-symlink directory: $DESTINATION_PARENT" >&2
    exit 1
}
readonly CANONICAL_PARENT=$(cd -- "$DESTINATION_PARENT" && pwd -P)
readonly DESTINATION="$CANONICAL_PARENT/$DESTINATION_NAME"

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

STAGE_DIR=$(mktemp -d "$CANONICAL_PARENT/.airbender-verifier-install.XXXXXX")
cleanup() {
    local exit_code=$?
    trap - EXIT INT TERM
    case "$STAGE_DIR" in
        "$CANONICAL_PARENT"/.airbender-verifier-install.*)
            rm -rf -- "$STAGE_DIR"
            ;;
        *)
            echo "refusing to clean unexpected installer staging path: $STAGE_DIR" >&2
            exit_code=1
            ;;
    esac
    exit "$exit_code"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

ARCHIVE="$STAGE_DIR/verifier-artifacts.tar.gz"
case "$SOURCE" in
    https://*)
        command -v curl >/dev/null 2>&1 || {
            echo "curl is required to fetch a verifier release" >&2
            exit 1
        }
        curl --fail --location --proto '=https' --tlsv1.2 \
            --output "$ARCHIVE" "$SOURCE"
        ;;
    *://*)
        echo "only HTTPS release URLs are accepted" >&2
        exit 1
        ;;
    *)
        [[ -f "$SOURCE" && ! -L "$SOURCE" ]] || {
            echo "archive must be a regular non-symlink file: $SOURCE" >&2
            exit 1
        }
        cp -p -- "$SOURCE" "$ARCHIVE"
        ;;
esac

[[ "$(sha256_file "$ARCHIVE")" == "$EXPECTED_ARCHIVE_SHA256" ]] || {
    echo "verifier release archive SHA-256 mismatch" >&2
    exit 1
}

EXPECTED_LIST="$STAGE_DIR/expected-files"
ARCHIVE_LIST="$STAGE_DIR/archive-files"
printf '%s\n' "${ARTIFACTS[@]}" | LC_ALL=C sort > "$EXPECTED_LIST"
tar -tzf "$ARCHIVE" > "$ARCHIVE_LIST"
LC_ALL=C sort -o "$ARCHIVE_LIST" "$ARCHIVE_LIST"
cmp "$EXPECTED_LIST" "$ARCHIVE_LIST" >/dev/null || {
    echo "verifier release archive does not contain the exact expected file set" >&2
    exit 1
}

PAYLOAD="$STAGE_DIR/payload"
mkdir "$PAYLOAD"
tar -xzf "$ARCHIVE" -C "$PAYLOAD"

for artifact in "${ARTIFACTS[@]}"; do
    [[ -f "$PAYLOAD/$artifact" && ! -L "$PAYLOAD/$artifact" ]] || {
        echo "release member must be a regular non-symlink file: $artifact" >&2
        exit 1
    }
done

[[ "$(sha256_file "$PAYLOAD/artifact-manifest.sha256")" == "$EXPECTED_MANIFEST_SHA256" ]] || {
    echo "verifier artifact manifest SHA-256 mismatch" >&2
    exit 1
}
[[ "$(sha256_file "$PINNED_MANIFEST")" == "$EXPECTED_MANIFEST_SHA256" ]] || {
    echo "requested manifest differs from the source-pinned verifier artifact table" >&2
    exit 1
}
cmp "$PINNED_MANIFEST" "$PAYLOAD/artifact-manifest.sha256" >/dev/null || {
    echo "release manifest differs from the source-pinned verifier artifact table" >&2
    exit 1
}

while read -r expected_hash artifact extra; do
    [[ -z "${extra:-}" && "$artifact" != */* ]] || {
        echo "invalid verifier artifact manifest record" >&2
        exit 1
    }
    is_sha256 "$expected_hash" || {
        echo "invalid SHA-256 in verifier artifact manifest" >&2
        exit 1
    }
    [[ -f "$PAYLOAD/$artifact" && ! -L "$PAYLOAD/$artifact" ]] || {
        echo "missing regular verifier artifact: $artifact" >&2
        exit 1
    }
    [[ "$(sha256_file "$PAYLOAD/$artifact")" == "$expected_hash" ]] || {
        echo "verifier artifact SHA-256 mismatch: $artifact" >&2
        exit 1
    }
done < "$PAYLOAD/artifact-manifest.sha256"

manifest_records=$(wc -l < "$PAYLOAD/artifact-manifest.sha256")
manifest_records=${manifest_records//[[:space:]]/}
[[ "$manifest_records" -eq 22 ]] || {
    echo "verifier artifact manifest must contain exactly 22 records" >&2
    exit 1
}

mv -- "$PAYLOAD" "$DESTINATION"
printf 'installed verifier release: %s\n' "$DESTINATION"
printf 'export AIRBENDER_VERIFIER_ARTIFACT_DIR=%q\n' "$DESTINATION"
printf 'export AIRBENDER_VERIFIER_ARTIFACT_MANIFEST_SHA256=%q\n' \
    "$EXPECTED_MANIFEST_SHA256"

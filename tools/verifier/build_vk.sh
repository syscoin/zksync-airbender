#!/usr/bin/env bash

set -euo pipefail

readonly VERIFIER_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly REPO_ROOT="$(cd -- "$VERIFIER_DIR/../.." && pwd)"
readonly BIN_DIR="${BIN_DIR:-$VERIFIER_DIR}"
readonly OUTPUT_DIR="${OUTPUT_DIR:-$VERIFIER_DIR}"
readonly VK_RUST_TOOLCHAIN=nightly-2025-07-25
readonly CLI_TARGET_DIR="$VERIFIER_DIR/target/vk_cli"
readonly CLI_BIN="$CLI_TARGET_DIR/release/cli"
readonly -a VK_NAMES=(
    universal.reduced.vk.json
    universal.reduced_log23.vk.json
    universal_no_delegation.final.vk.json
    base_layer.reduced.vk.json
    recursion_layer.reduced.vk.json
    recursion_log_23_layer.reduced.vk.json
    recursion_layer_no_delegation.final.vk.json
    final_recursion_layer.final.vk.json
)

export LC_ALL=C
export CARGO_INCREMENTAL=0

VK_STAGE_DIR=""
VK_BACKUP_DIR=""
VK_PUBLISH_STARTED=false
VK_PUBLISH_COMMITTED=false
VK_LOCK_ACQUIRED=false

cleanup() {
    local exit_code=$?
    trap - EXIT INT TERM
    local rollback_failed=false
    local cleanup_failed=false

    if [[ "$VK_PUBLISH_STARTED" == "true" && "$VK_PUBLISH_COMMITTED" != "true" ]]; then
        local vk restore_tmp
        for vk in "${VK_NAMES[@]}"; do
            if [[ -f "$VK_BACKUP_DIR/$vk" ]]; then
                restore_tmp=""
                if restore_tmp=$(mktemp "$OUTPUT_DIR/.vk-restore.XXXXXX") \
                    && cp -p -- "$VK_BACKUP_DIR/$vk" "$restore_tmp" \
                    && mv -f -- "$restore_tmp" "$OUTPUT_DIR/$vk"; then
                    :
                else
                    [[ -z "$restore_tmp" ]] || rm -f -- "$restore_tmp" 2>/dev/null || true
                    echo "failed to restore verification key: $vk" >&2
                    rollback_failed=true
                fi
            elif [[ -f "$VK_BACKUP_DIR/.absent-$vk" ]]; then
                if ! rm -f -- "$OUTPUT_DIR/$vk"; then
                    echo "failed to remove newly published verification key: $vk" >&2
                    rollback_failed=true
                fi
            fi
        done
    fi

    if [[ -n "$VK_BACKUP_DIR" && "$rollback_failed" != "true" ]]; then
        case "$VK_BACKUP_DIR" in
            "$OUTPUT_DIR"/.vk-backup.*)
                if ! rm -rf -- "$VK_BACKUP_DIR"; then
                    echo "failed to clean VK backup: $VK_BACKUP_DIR" >&2
                    cleanup_failed=true
                fi
                ;;
            *) echo "refusing to clean unexpected VK backup path: $VK_BACKUP_DIR" >&2 ;;
        esac
    fi
    if [[ -n "$VK_STAGE_DIR" ]]; then
        case "$VK_STAGE_DIR" in
            "$OUTPUT_DIR"/.vk-stage.*)
                if ! rm -rf -- "$VK_STAGE_DIR"; then
                    echo "failed to clean VK staging path: $VK_STAGE_DIR" >&2
                    cleanup_failed=true
                fi
                ;;
            *) echo "refusing to clean unexpected VK staging path: $VK_STAGE_DIR" >&2 ;;
        esac
    fi

    if [[ "$rollback_failed" == "true" ]]; then
        echo "VK rollback failed; preserving backup and publish lock for manual recovery: $VK_BACKUP_DIR" >&2
        exit_code=1
    elif [[ "$VK_LOCK_ACQUIRED" == "true" ]]; then
        if ! rmdir "$VERIFIER_DIR/.artifact-publish.lock"; then
            echo "failed to release VK publish lock: $VERIFIER_DIR/.artifact-publish.lock" >&2
            cleanup_failed=true
        fi
    fi

    if [[ "$cleanup_failed" == "true" && "$exit_code" -eq 0 ]]; then
        exit_code=1
    fi
    exit "$exit_code"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if [[ "$OUTPUT_DIR" == "$VERIFIER_DIR" \
    && "${ARTIFACT_PUBLISH_LOCK_HELD:-0}" != "1" ]]; then
    if ! mkdir "$VERIFIER_DIR/.artifact-publish.lock" 2>/dev/null; then
        echo "another verifier artifact publisher holds $VERIFIER_DIR/.artifact-publish.lock" >&2
        exit 1
    fi
    VK_LOCK_ACQUIRED=true
fi

[[ -d "$BIN_DIR" ]] || {
    echo "verifier binary directory does not exist: $BIN_DIR" >&2
    exit 1
}
mkdir -p -- "$OUTPUT_DIR"

# SYSCOIN: Refuse to derive a VK from a stale or absent verifier binary.
readonly -a VK_INPUTS=(
    universal.bin
    universal_no_delegation.bin
    base_layer.bin
    recursion_layer.bin
    recursion_layer_no_delegation.bin
    final_recursion_layer.bin
)
for binary in "${VK_INPUTS[@]}"; do
    [[ -s "$BIN_DIR/$binary" ]] || {
        echo "missing or empty VK input binary: $BIN_DIR/$binary" >&2
        exit 1
    }
done

(
    cd "$REPO_ROOT"
    CARGO_TARGET_DIR="$CLI_TARGET_DIR" \
        cargo +"$VK_RUST_TOOLCHAIN" build --locked --release --no-default-features \
            --features vk_generation_without_artifacts \
            -p cli
)
[[ -x "$CLI_BIN" ]] || {
    echo "VK generator was not built: $CLI_BIN" >&2
    exit 1
}

VK_STAGE_DIR="$(mktemp -d "$OUTPUT_DIR/.vk-stage.XXXXXX")"

generate_vk() {
    local label=$1
    local binary=$2
    local machine=$3
    local output=$4

    printf 'generating VK: %s\n' "$label"
    "$CLI_BIN" generate-vk \
        --bin "$BIN_DIR/$binary" \
        --machine "$machine" \
        --output "$VK_STAGE_DIR/$output"
}

# SYSCOIN: Key generation is deliberately sequential. Each process can use
# substantial memory; parallel jobs can be OOM-killed after valid ROMs were
# built. `set -e` makes every individual command fail closed.
generate_vk universal-reduced universal.bin reduced universal.reduced.vk.json
generate_vk universal-log23 universal.bin reduced-log23 universal.reduced_log23.vk.json
generate_vk universal-final universal_no_delegation.bin reduced-final universal_no_delegation.final.vk.json
generate_vk base-reduced base_layer.bin reduced base_layer.reduced.vk.json
generate_vk recursion-reduced recursion_layer.bin reduced recursion_layer.reduced.vk.json

# This is intentional: the terminal ReducedLog23 proof proves the ordinary
# recursion verifier program on the log23 machine. recursion_log_23_layer.bin
# is the verifier executable for those proofs, not the program being proven.
generate_vk recursion-log23 recursion_layer.bin reduced-log23 recursion_log_23_layer.reduced.vk.json

generate_vk recursion-final recursion_layer_no_delegation.bin reduced-final recursion_layer_no_delegation.final.vk.json
generate_vk final-recursion final_recursion_layer.bin reduced-final final_recursion_layer.final.vk.json

for vk in "${VK_NAMES[@]}"; do
    [[ -s "$VK_STAGE_DIR/$vk" ]] || {
        echo "missing or empty generated VK: $vk" >&2
        exit 1
    }
    grep -q '"bytecode_hash_hex"' "$VK_STAGE_DIR/$vk" || {
        echo "generated VK lacks bytecode hash: $vk" >&2
        exit 1
    }
done

if [[ "$OUTPUT_DIR" == "$VERIFIER_DIR" ]]; then
    VK_BACKUP_DIR="$(mktemp -d "$OUTPUT_DIR/.vk-backup.XXXXXX")"
    for vk in "${VK_NAMES[@]}"; do
        if [[ -e "$OUTPUT_DIR/$vk" ]]; then
            cp -p -- "$OUTPUT_DIR/$vk" "$VK_BACKUP_DIR/$vk"
        else
            : > "$VK_BACKUP_DIR/.absent-$vk"
        fi
    done
    VK_PUBLISH_STARTED=true
fi

for vk in "${VK_NAMES[@]}"; do
    mv -f -- "$VK_STAGE_DIR/$vk" "$OUTPUT_DIR/$vk"
done
VK_PUBLISH_COMMITTED=true

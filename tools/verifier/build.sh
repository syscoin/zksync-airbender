#!/usr/bin/env bash

set -euo pipefail

# SYSCOIN: Build every verifier into an isolated staging directory. A failed
# background job must never leave an old binary or VK looking canonical.
readonly VERIFIER_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly MAX_ROM_SIZE=2097152
readonly DEFAULT_CARGO_PROFILE=release
readonly UNIVERSAL_NO_DELEGATION_CARGO_PROFILE=release-universal-no-delegation
readonly -a BINARIES=(
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
)
readonly -a VKS=(
    universal.reduced.vk.json
    universal.reduced_log23.vk.json
    universal_no_delegation.final.vk.json
    base_layer.reduced.vk.json
    recursion_layer.reduced.vk.json
    recursion_log_23_layer.reduced.vk.json
    recursion_layer_no_delegation.final.vk.json
    final_recursion_layer.final.vk.json
)
readonly -a BUILD_METADATA=(
    build-profiles.tsv
)

cd "$VERIFIER_DIR"
export LC_ALL=C
export CARGO_INCREMENTAL=0

PUBLISH_LOCK_DIR="$VERIFIER_DIR/.artifact-publish.lock"
PUBLISH_LOCK_ACQUIRED=false
STAGE_DIR=""
BACKUP_DIR=""
PUBLISH_STARTED=false
PUBLISH_COMMITTED=false
declare -a BUILD_PIDS=()
declare -a BUILD_LABELS=()

cleanup() {
    local exit_code=$?
    trap - EXIT INT TERM

    local rollback_failed=false
    local cleanup_failed=false

    # SYSCOIN: Reap every child so a failed foreground build cannot leave
    # concurrent cargo jobs writing after this script exits.
    local pid
    for pid in "${BUILD_PIDS[@]:-}"; do
        kill "$pid" 2>/dev/null || true
    done
    for pid in "${BUILD_PIDS[@]:-}"; do
        wait "$pid" 2>/dev/null || true
    done

    # SYSCOIN: Roll back every previously published artifact if an individual
    # rename, signal, or disk error interrupts the multi-file publication.
    if [[ "$PUBLISH_STARTED" == "true" && "$PUBLISH_COMMITTED" != "true" ]]; then
        local artifact restore_tmp
        for artifact in "${BINARIES[@]}" "${VKS[@]}" "${BUILD_METADATA[@]}" \
            artifact-manifest.sha256 artifact-manifest.tsv; do
            if [[ -f "$BACKUP_DIR/$artifact" ]]; then
                restore_tmp=""
                if restore_tmp=$(mktemp "$VERIFIER_DIR/.artifact-restore.XXXXXX") \
                    && cp -p -- "$BACKUP_DIR/$artifact" "$restore_tmp" \
                    && mv -f -- "$restore_tmp" "$VERIFIER_DIR/$artifact"; then
                    :
                else
                    [[ -z "$restore_tmp" ]] || rm -f -- "$restore_tmp" 2>/dev/null || true
                    echo "failed to restore verifier artifact: $artifact" >&2
                    rollback_failed=true
                fi
            elif [[ -f "$BACKUP_DIR/.absent-$artifact" ]]; then
                if ! rm -f -- "$VERIFIER_DIR/$artifact"; then
                    echo "failed to remove newly published verifier artifact: $artifact" >&2
                    rollback_failed=true
                fi
            fi
        done
    fi

    if [[ -n "$BACKUP_DIR" && "$rollback_failed" != "true" ]]; then
        case "$BACKUP_DIR" in
            "$VERIFIER_DIR"/.artifact-backup.*)
                if ! rm -rf -- "$BACKUP_DIR"; then
                    echo "failed to clean verifier artifact backup: $BACKUP_DIR" >&2
                    cleanup_failed=true
                fi
                ;;
            *) echo "refusing to clean unexpected backup path: $BACKUP_DIR" >&2 ;;
        esac
    fi

    if [[ -n "$STAGE_DIR" ]]; then
        case "$STAGE_DIR" in
            "$VERIFIER_DIR"/.build-stage.*)
                if ! rm -rf -- "$STAGE_DIR"; then
                    echo "failed to clean verifier staging path: $STAGE_DIR" >&2
                    cleanup_failed=true
                fi
                ;;
            *) echo "refusing to clean unexpected staging path: $STAGE_DIR" >&2 ;;
        esac
    fi

    if [[ "$rollback_failed" == "true" ]]; then
        echo "verifier rollback failed; preserving backup and publish lock for manual recovery: $BACKUP_DIR" >&2
        exit_code=1
    elif [[ "$PUBLISH_LOCK_ACQUIRED" == "true" ]]; then
        if ! rmdir "$PUBLISH_LOCK_DIR"; then
            echo "failed to release verifier publish lock: $PUBLISH_LOCK_DIR" >&2
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

if ! mkdir "$PUBLISH_LOCK_DIR" 2>/dev/null; then
    echo "another verifier artifact publisher holds $PUBLISH_LOCK_DIR" >&2
    exit 1
fi
PUBLISH_LOCK_ACQUIRED=true
STAGE_DIR="$(mktemp -d "$VERIFIER_DIR/.build-stage.XXXXXX")"
readonly BUILD_PROFILES_PATH="$STAGE_DIR/build-profiles.tsv"
printf 'path\tcargo_profile\tfeatures\tbuild_std_mode\n' > "$BUILD_PROFILES_PATH"

bash "$VERIFIER_DIR/test_linker_data_assertion.sh"

record_build_profile() {
    local output=$1
    local profile=$2
    local features=$3
    local build_std=$4

    printf '%s\t%s\t%s\t%s\n' \
        "$output" "$profile" "$features" "$build_std" >> "$BUILD_PROFILES_PATH"
}

start_objcopy() {
    local label=$1
    local target_dir=$2
    local features=$3
    local build_std=$4
    local output=$5
    local profile=${6:-$DEFAULT_CARGO_PROFILE}

    record_build_profile "$output" "$profile" "$features" "$build_std"

    (
        local -a args=(objcopy --locked)
        if [[ "$profile" == "$DEFAULT_CARGO_PROFILE" ]]; then
            args+=(--release)
        else
            args+=(--profile "$profile")
        fi
        if [[ "$build_std" != "no" ]]; then
            args+=(
                -Z build-std=core,panic_abort,alloc
            )
        fi
        if [[ "$build_std" == "immediate" ]]; then
            args+=(-Z build-std-features=panic_immediate_abort)
        fi
        args+=(--features "$features" --no-default-features)

        CARGO_TARGET_DIR="$VERIFIER_DIR/target/$target_dir" \
            cargo "${args[@]}" -- -O binary "$STAGE_DIR/$output"
    ) &
    BUILD_PIDS+=("$!")
    BUILD_LABELS+=("$label")
}

wait_for_objcopy_jobs() {
    local failed=0
    local i
    for ((i = 0; i < ${#BUILD_PIDS[@]}; i++)); do
        if ! wait "${BUILD_PIDS[$i]}"; then
            echo "verifier build failed: ${BUILD_LABELS[$i]}" >&2
            failed=1
        fi
    done
    BUILD_PIDS=()
    BUILD_LABELS=()
    return "$failed"
}

# SYSCOIN: Preserve upstream feature/target separation while tracking every
# concurrent process explicitly; a bare `wait` can hide an earlier failure.
start_objcopy base-layer one base_layer immediate base_layer.bin
start_objcopy recursion-layer two recursion_step immediate recursion_layer.bin
start_objcopy recursion-log23-layer three recursion_log_23_step immediate recursion_log_23_layer.bin
start_objcopy recursion-no-delegation four recursion_step_no_delegation immediate recursion_layer_no_delegation.bin
start_objcopy final-recursion-layer five final_recursion_step immediate final_recursion_layer.bin
start_objcopy base-layer-output six base_layer,panic_output no base_layer_with_output.bin
start_objcopy recursion-layer-output seven recursion_step,panic_output no recursion_layer_with_output.bin
start_objcopy recursion-log23-output eight recursion_log_23_step,panic_output no recursion_log_23_layer_with_output.bin
start_objcopy recursion-no-delegation-output nine recursion_step_no_delegation,panic_output no recursion_layer_no_delegation_with_output.bin
start_objcopy final-recursion-output ten final_recursion_step,panic_output no final_recursion_layer_with_output.bin

# Keep the largest link in the foreground to preserve the upstream memory
# profile while the smaller verifier jobs finish in parallel.
record_build_profile \
    universal.bin "$DEFAULT_CARGO_PROFILE" universal_circuit,panic_output plain
CARGO_TARGET_DIR="$VERIFIER_DIR/target/eleven" \
    cargo objcopy --locked --release \
    -Z build-std=core,panic_abort,alloc \
    --features universal_circuit,panic_output --no-default-features \
    -- -O binary "$STAGE_DIR/universal.bin"

start_objcopy \
    universal-no-delegation \
    twelve \
    universal_circuit_no_delegation,panic_output \
    plain \
    universal_no_delegation.bin \
    "$UNIVERSAL_NO_DELEGATION_CARGO_PROFILE"
wait_for_objcopy_jobs

# SYSCOIN: Compare the complete build recipe byte-for-byte. This makes a
# feature, build-std, output-order, or profile change a consensus-visible
# failure instead of merely recording potentially stale provenance.
EXPECTED_BUILD_PROFILES_PATH="$STAGE_DIR/.expected-build-profiles.tsv"
{
    printf 'path\tcargo_profile\tfeatures\tbuild_std_mode\n'
    printf 'base_layer.bin\trelease\tbase_layer\timmediate\n'
    printf 'recursion_layer.bin\trelease\trecursion_step\timmediate\n'
    printf 'recursion_log_23_layer.bin\trelease\trecursion_log_23_step\timmediate\n'
    printf 'recursion_layer_no_delegation.bin\trelease\trecursion_step_no_delegation\timmediate\n'
    printf 'final_recursion_layer.bin\trelease\tfinal_recursion_step\timmediate\n'
    printf 'base_layer_with_output.bin\trelease\tbase_layer,panic_output\tno\n'
    printf 'recursion_layer_with_output.bin\trelease\trecursion_step,panic_output\tno\n'
    printf 'recursion_log_23_layer_with_output.bin\trelease\trecursion_log_23_step,panic_output\tno\n'
    printf 'recursion_layer_no_delegation_with_output.bin\trelease\trecursion_step_no_delegation,panic_output\tno\n'
    printf 'final_recursion_layer_with_output.bin\trelease\tfinal_recursion_step,panic_output\tno\n'
    printf 'universal.bin\trelease\tuniversal_circuit,panic_output\tplain\n'
    printf 'universal_no_delegation.bin\trelease-universal-no-delegation\tuniversal_circuit_no_delegation,panic_output\tplain\n'
} > "$EXPECTED_BUILD_PROFILES_PATH"
if ! cmp "$EXPECTED_BUILD_PROFILES_PATH" "$BUILD_PROFILES_PATH"; then
    echo "verifier build recipe differs from the canonical 12-artifact profile map" >&2
    exit 1
fi
rm -f -- "$EXPECTED_BUILD_PROFILES_PATH"

validate_binary() {
    local path=$1
    local size

    [[ -s "$path" ]] || {
        echo "missing or empty verifier binary: $path" >&2
        return 1
    }
    size=$(wc -c < "$path")
    size=${size//[[:space:]]/}
    ((size <= MAX_ROM_SIZE)) || {
        echo "verifier binary exceeds ${MAX_ROM_SIZE}-byte ROM: $path ($size bytes)" >&2
        return 1
    }
    ((size % 4 == 0)) || {
        echo "verifier binary is not word-aligned: $path ($size bytes)" >&2
        return 1
    }
}

for binary in "${BINARIES[@]}"; do
    validate_binary "$STAGE_DIR/$binary"
done

# SYSCOIN: Generate VKs from the just-built staged bytecode. The misleading
# recursion_log_23 VK name intentionally binds recursion_layer.bin under the
# ReducedLog23 machine; execution_utils uses that exact terminal program.
ARTIFACT_PUBLISH_LOCK_HELD=1 BIN_DIR="$STAGE_DIR" OUTPUT_DIR="$STAGE_DIR" \
    "$VERIFIER_DIR/build_vk.sh"

for vk in "${VKS[@]}"; do
    [[ -s "$STAGE_DIR/$vk" ]] || {
        echo "missing or empty verification key: $vk" >&2
        exit 1
    }
    grep -q '"bytecode_hash_hex"' "$STAGE_DIR/$vk" || {
        echo "verification key lacks bytecode hash: $vk" >&2
        exit 1
    }
done

# SYSCOIN: Freeze both a machine-readable checksum list and a human-friendly
# size/checksum table before replacing any checked-in artifact.
: > "$STAGE_DIR/artifact-manifest.sha256"
printf 'path\tsize_bytes\tsha256\n' > "$STAGE_DIR/artifact-manifest.tsv"
sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}
for artifact in "${BINARIES[@]}" "${VKS[@]}" "${BUILD_METADATA[@]}"; do
    size=$(wc -c < "$STAGE_DIR/$artifact")
    size=${size//[[:space:]]/}
    hash=$(sha256_file "$STAGE_DIR/$artifact")
    printf '%s  %s\n' "$hash" "$artifact" >> "$STAGE_DIR/artifact-manifest.sha256"
    printf '%s\t%s\t%s\n' "$artifact" "$size" "$hash" >> "$STAGE_DIR/artifact-manifest.tsv"
done

while read -r expected_hash artifact; do
    [[ "$(sha256_file "$STAGE_DIR/$artifact")" == "$expected_hash" ]] || {
        echo "staged artifact checksum changed before publication: $artifact" >&2
        exit 1
    }
done < "$STAGE_DIR/artifact-manifest.sha256"

BACKUP_DIR="$(mktemp -d "$VERIFIER_DIR/.artifact-backup.XXXXXX")"
for artifact in "${BINARIES[@]}" "${VKS[@]}" "${BUILD_METADATA[@]}" \
    artifact-manifest.sha256 artifact-manifest.tsv; do
    if [[ -e "$VERIFIER_DIR/$artifact" ]]; then
        cp -p -- "$VERIFIER_DIR/$artifact" "$BACKUP_DIR/$artifact"
    else
        : > "$BACKUP_DIR/.absent-$artifact"
    fi
done
PUBLISH_STARTED=true

for artifact in "${BINARIES[@]}" "${VKS[@]}" "${BUILD_METADATA[@]}" \
    artifact-manifest.sha256 artifact-manifest.tsv; do
    mv -f -- "$STAGE_DIR/$artifact" "$VERIFIER_DIR/$artifact"
done
PUBLISH_COMMITTED=true

echo "published ${#BINARIES[@]} verifier binaries and ${#VKS[@]} VKs"

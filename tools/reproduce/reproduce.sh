#!/usr/bin/env bash

set -euo pipefail

# SYSCOIN: Canonical verifier reproduction is clean-tree-only by default. Set
# ALLOW_DIRTY_REPRODUCE=1 solely for noncanonical development measurements.
readonly REPO_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
readonly VERIFIER_DIR="$REPO_ROOT/tools/verifier"
readonly CONTAINER_VERIFIER_DIR=/zksync-airbender/tools/verifier
readonly MAX_ROM_SIZE=2097152
readonly REPRODUCTION_MANIFEST_PATH="${REPRODUCTION_MANIFEST_PATH:-$VERIFIER_DIR/reproduction-manifest.tsv}"
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
readonly -a METADATA_FILES=(
    build-profiles.tsv
    artifact-manifest.sha256
    artifact-manifest.tsv
    build-environment.txt
)

cd "$REPO_ROOT"
export LC_ALL=C
export DOCKER_DEFAULT_PLATFORM=linux/amd64

command -v docker >/dev/null 2>&1 || {
    echo "docker is required for verifier reproduction" >&2
    exit 1
}

# SYSCOIN: Per-run provenance is configurable, but it must never alias or
# overwrite a canonical verifier artifact.
reproduction_manifest_parent=$(dirname -- "$REPRODUCTION_MANIFEST_PATH")
[[ -d "$reproduction_manifest_parent" ]] || {
    echo "reproduction manifest directory does not exist: $reproduction_manifest_parent" >&2
    exit 1
}
REPRODUCTION_MANIFEST_DIRECTORY=$(cd -- "$reproduction_manifest_parent" && pwd -P)
reproduction_manifest_name=$(basename -- "$REPRODUCTION_MANIFEST_PATH")
REPRODUCTION_MANIFEST_DESTINATION="$REPRODUCTION_MANIFEST_DIRECTORY/$reproduction_manifest_name"
if [[ -L "$REPRODUCTION_MANIFEST_DESTINATION" \
    || ( -e "$REPRODUCTION_MANIFEST_DESTINATION" \
        && ! -f "$REPRODUCTION_MANIFEST_DESTINATION" ) ]]; then
    echo "reproduction manifest target must be a regular non-symlink file: $REPRODUCTION_MANIFEST_DESTINATION" >&2
    exit 1
fi
for canonical_artifact in "${BINARIES[@]}" "${VKS[@]}" "${METADATA_FILES[@]}"; do
    if [[ "$REPRODUCTION_MANIFEST_DESTINATION" == "$VERIFIER_DIR/$canonical_artifact" ]]; then
        echo "reproduction manifest target collides with canonical artifact: $canonical_artifact" >&2
        exit 1
    fi
done

source_status=$(git status --porcelain --untracked-files=all)
if [[ -n "$source_status" && "${ALLOW_DIRTY_REPRODUCE:-0}" != "1" ]]; then
    echo "canonical verifier reproduction requires a clean source tree" >&2
    echo "commit the frozen patch, or use ALLOW_DIRTY_REPRODUCE=1 for a noncanonical development build" >&2
    exit 1
fi

readonly REPO_HEAD="$(git rev-parse HEAD)"
readonly REPO_TREE="$(git rev-parse 'HEAD^{tree}')"
# SYSCOIN: Match the deterministic dependency commit date used by the server
# launcher. Deriving this from HEAD would make an artifact commit change its
# own build environment on the next reproduction run.
readonly SOURCE_DATE_EPOCH=946684800
readonly DIRTY_SOURCE="$([[ -n "$source_status" ]] && printf true || printf false)"
readonly IMAGE_TAG="airbender-verifiers-reproduce:${REPO_HEAD:0:12}-$$"

STAGE_DIR=""
PUBLISH_DIR=""
PUBLISH_BACKUP_DIR=""
PUBLISH_STARTED=false
PUBLISH_COMMITTED=false
PUBLISH_LOCK_ACQUIRED=false
CONTAINER_ID=""
REPRODUCTION_MANIFEST_TMP=""

cleanup() {
    local exit_code=$?
    trap - EXIT INT TERM
    local rollback_failed=false
    local cleanup_failed=false

    if [[ -n "$CONTAINER_ID" ]]; then
        docker rm -f "$CONTAINER_ID" >/dev/null 2>&1 || true
    fi
    if [[ "${KEEP_REPRODUCTION_IMAGE:-0}" != "1" ]]; then
        docker image rm "$IMAGE_TAG" >/dev/null 2>&1 || true
    fi
    if [[ "$PUBLISH_STARTED" == "true" && "$PUBLISH_COMMITTED" != "true" ]]; then
        local artifact restore_tmp manifest_restore_tmp
        for artifact in "${BINARIES[@]}" "${VKS[@]}" "${METADATA_FILES[@]}"; do
            if [[ -f "$PUBLISH_BACKUP_DIR/$artifact" ]]; then
                restore_tmp=""
                if restore_tmp=$(mktemp "$VERIFIER_DIR/.artifact-restore.XXXXXX") \
                    && cp -p -- "$PUBLISH_BACKUP_DIR/$artifact" "$restore_tmp" \
                    && mv -f -- "$restore_tmp" "$VERIFIER_DIR/$artifact"; then
                    :
                else
                    [[ -z "$restore_tmp" ]] || rm -f -- "$restore_tmp" 2>/dev/null || true
                    echo "failed to restore reproduced artifact: $artifact" >&2
                    rollback_failed=true
                fi
            elif [[ -f "$PUBLISH_BACKUP_DIR/.absent-$artifact" ]]; then
                if ! rm -f -- "$VERIFIER_DIR/$artifact"; then
                    echo "failed to remove newly reproduced artifact: $artifact" >&2
                    rollback_failed=true
                fi
            fi
        done

        if [[ -f "$PUBLISH_BACKUP_DIR/reproduction-manifest.external" ]]; then
            manifest_restore_tmp=""
            if manifest_restore_tmp=$(mktemp "$REPRODUCTION_MANIFEST_DIRECTORY/.reproduction-restore.XXXXXX") \
                && cp -p -- "$PUBLISH_BACKUP_DIR/reproduction-manifest.external" "$manifest_restore_tmp" \
                && mv -f -- "$manifest_restore_tmp" "$REPRODUCTION_MANIFEST_DESTINATION"; then
                :
            else
                [[ -z "$manifest_restore_tmp" ]] \
                    || rm -f -- "$manifest_restore_tmp" 2>/dev/null \
                    || true
                echo "failed to restore reproduction provenance: $REPRODUCTION_MANIFEST_DESTINATION" >&2
                rollback_failed=true
            fi
        elif [[ -f "$PUBLISH_BACKUP_DIR/.absent-reproduction-manifest.external" ]]; then
            if ! rm -f -- "$REPRODUCTION_MANIFEST_DESTINATION"; then
                echo "failed to remove newly published reproduction provenance: $REPRODUCTION_MANIFEST_DESTINATION" >&2
                rollback_failed=true
            fi
        fi
    fi

    if [[ -n "$REPRODUCTION_MANIFEST_TMP" ]]; then
        rm -f -- "$REPRODUCTION_MANIFEST_TMP" 2>/dev/null || true
    fi
    if [[ -n "$PUBLISH_BACKUP_DIR" && "$rollback_failed" != "true" ]]; then
        case "$PUBLISH_BACKUP_DIR" in
            "$VERIFIER_DIR"/.artifact-backup.*)
                if ! rm -rf -- "$PUBLISH_BACKUP_DIR"; then
                    echo "failed to clean reproduction backup: $PUBLISH_BACKUP_DIR" >&2
                    cleanup_failed=true
                fi
                ;;
            *) echo "refusing to clean unexpected backup path: $PUBLISH_BACKUP_DIR" >&2 ;;
        esac
    fi
    if [[ -n "$PUBLISH_DIR" ]]; then
        case "$PUBLISH_DIR" in
            "$VERIFIER_DIR"/.reproduce-publish.*)
                if ! rm -rf -- "$PUBLISH_DIR"; then
                    echo "failed to clean reproduction publish path: $PUBLISH_DIR" >&2
                    cleanup_failed=true
                fi
                ;;
            *) echo "refusing to clean unexpected publish path: $PUBLISH_DIR" >&2 ;;
        esac
    fi
    if [[ -n "$STAGE_DIR" ]]; then
        case "$STAGE_DIR" in
            "${TMPDIR:-/tmp}"/airbender-reproduce.*)
                if ! rm -rf -- "$STAGE_DIR"; then
                    echo "failed to clean reproduction staging path: $STAGE_DIR" >&2
                    cleanup_failed=true
                fi
                ;;
            *) echo "refusing to clean unexpected reproduction path: $STAGE_DIR" >&2 ;;
        esac
    fi

    if [[ "$rollback_failed" == "true" ]]; then
        echo "reproduction rollback failed; preserving backup and publish lock for manual recovery: $PUBLISH_BACKUP_DIR" >&2
        exit_code=1
    elif [[ "$PUBLISH_LOCK_ACQUIRED" == "true" ]]; then
        if ! rmdir "$VERIFIER_DIR/.artifact-publish.lock"; then
            echo "failed to release reproduction publish lock: $VERIFIER_DIR/.artifact-publish.lock" >&2
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

if ! mkdir "$VERIFIER_DIR/.artifact-publish.lock" 2>/dev/null; then
    echo "another verifier artifact publisher holds $VERIFIER_DIR/.artifact-publish.lock" >&2
    exit 1
fi
PUBLISH_LOCK_ACQUIRED=true
STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/airbender-reproduce.XXXXXX")"

sha256_file() {
    local path=$1
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$path" | awk '{print $1}'
    else
        shasum -a 256 "$path" | awk '{print $1}'
    fi
}

file_size() {
    local size
    size=$(wc -c < "$1")
    printf '%s' "${size//[[:space:]]/}"
}

# SYSCOIN: Always rebuild without Docker layer cache for a canonical run. The
# pinned inputs should reproduce byte-for-byte without relying on an old image.
docker build \
    --no-cache \
    --platform linux/amd64 \
    --progress plain \
    --build-arg SOURCE_DATE_EPOCH="$SOURCE_DATE_EPOCH" \
    --iidfile "$STAGE_DIR/image.id" \
    --tag "$IMAGE_TAG" \
    --file tools/reproduce/Dockerfile \
    .

readonly IMAGE_ID="$(tr -d '[:space:]' < "$STAGE_DIR/image.id")"
[[ -n "$IMAGE_ID" ]] || {
    echo "docker did not report the built image ID" >&2
    exit 1
}

CONTAINER_ID="$(docker create --platform linux/amd64 "$IMAGE_ID")"
[[ -n "$CONTAINER_ID" ]] || {
    echo "docker did not create a verifier artifact container" >&2
    exit 1
}

for artifact in "${BINARIES[@]}" "${VKS[@]}" "${METADATA_FILES[@]}"; do
    docker cp \
        "$CONTAINER_ID:$CONTAINER_VERIFIER_DIR/$artifact" \
        "$STAGE_DIR/$artifact"
    [[ -s "$STAGE_DIR/$artifact" ]] || {
        echo "missing or empty reproduced artifact: $artifact" >&2
        exit 1
    }
done

# SYSCOIN: Independently require the exact 12-artifact build recipe before
# trusting or publishing its provenance.
EXPECTED_BUILD_PROFILES_PATH="$STAGE_DIR/expected-build-profiles.tsv"
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
if ! cmp "$EXPECTED_BUILD_PROFILES_PATH" "$STAGE_DIR/build-profiles.tsv"; then
    echo "reproduced verifier build recipe differs from the canonical profile map" >&2
    exit 1
fi
rm -f -- "$EXPECTED_BUILD_PROFILES_PATH"

# SYSCOIN: Independently reconstruct both manifests on the host. This catches
# stale image contents, truncated docker copies, missing files, and extra files.
: > "$STAGE_DIR/host-artifact-manifest.sha256"
printf 'path\tsize_bytes\tsha256\n' > "$STAGE_DIR/host-artifact-manifest.tsv"
for artifact in "${BINARIES[@]}" "${VKS[@]}"; do
    size=$(file_size "$STAGE_DIR/$artifact")
    hash=$(sha256_file "$STAGE_DIR/$artifact")

    if [[ "$artifact" == *.bin ]]; then
        ((size <= MAX_ROM_SIZE)) || {
            echo "reproduced binary exceeds ${MAX_ROM_SIZE}-byte ROM: $artifact ($size bytes)" >&2
            exit 1
        }
        ((size % 4 == 0)) || {
            echo "reproduced binary is not word-aligned: $artifact ($size bytes)" >&2
            exit 1
        }
    else
        grep -q '"bytecode_hash_hex"' "$STAGE_DIR/$artifact" || {
            echo "reproduced VK lacks bytecode hash: $artifact" >&2
            exit 1
        }
    fi

    printf '%s  %s\n' "$hash" "$artifact" >> "$STAGE_DIR/host-artifact-manifest.sha256"
    printf '%s\t%s\t%s\n' "$artifact" "$size" "$hash" >> "$STAGE_DIR/host-artifact-manifest.tsv"
done

profiles_size=$(file_size "$STAGE_DIR/build-profiles.tsv")
profiles_hash=$(sha256_file "$STAGE_DIR/build-profiles.tsv")
printf '%s  %s\n' "$profiles_hash" build-profiles.tsv \
    >> "$STAGE_DIR/host-artifact-manifest.sha256"
printf '%s\t%s\t%s\n' build-profiles.tsv "$profiles_size" "$profiles_hash" \
    >> "$STAGE_DIR/host-artifact-manifest.tsv"

environment_size=$(file_size "$STAGE_DIR/build-environment.txt")
environment_hash=$(sha256_file "$STAGE_DIR/build-environment.txt")
printf '%s  %s\n' "$environment_hash" build-environment.txt \
    >> "$STAGE_DIR/host-artifact-manifest.sha256"
printf '%s\t%s\t%s\n' build-environment.txt "$environment_size" "$environment_hash" \
    >> "$STAGE_DIR/host-artifact-manifest.tsv"

cmp "$STAGE_DIR/artifact-manifest.sha256" "$STAGE_DIR/host-artifact-manifest.sha256"
cmp "$STAGE_DIR/artifact-manifest.tsv" "$STAGE_DIR/host-artifact-manifest.tsv"

artifact_manifest_hash=$(sha256_file "$STAGE_DIR/artifact-manifest.sha256")
printf 'key\tvalue\n' > "$STAGE_DIR/reproduction-manifest.tsv"
printf 'repository_head\t%s\n' "$REPO_HEAD" >> "$STAGE_DIR/reproduction-manifest.tsv"
printf 'repository_tree\t%s\n' "$REPO_TREE" >> "$STAGE_DIR/reproduction-manifest.tsv"
printf 'source_tree_dirty\t%s\n' "$DIRTY_SOURCE" >> "$STAGE_DIR/reproduction-manifest.tsv"
printf 'source_date_epoch\t%s\n' "$SOURCE_DATE_EPOCH" >> "$STAGE_DIR/reproduction-manifest.tsv"
printf 'platform\tlinux/amd64\n' >> "$STAGE_DIR/reproduction-manifest.tsv"
printf 'docker_image_id\t%s\n' "$IMAGE_ID" >> "$STAGE_DIR/reproduction-manifest.tsv"
printf 'artifact_manifest_sha256\t%s\n' "$artifact_manifest_hash" >> "$STAGE_DIR/reproduction-manifest.tsv"
printf 'build_profiles_sha256\t%s\n' "$profiles_hash" >> "$STAGE_DIR/reproduction-manifest.tsv"
printf 'build_environment_sha256\t%s\n' "$environment_hash" >> "$STAGE_DIR/reproduction-manifest.tsv"

# SYSCOIN: A long Docker build must not publish into a source checkout that
# changed after its context was captured. Canonical runs begin clean, so this
# also binds the image inputs to REPO_HEAD/REPO_TREE.
current_head=$(git rev-parse HEAD)
current_source_status=$(git status --porcelain --untracked-files=all)
if [[ "$current_head" != "$REPO_HEAD" || "$current_source_status" != "$source_status" ]]; then
    echo "source checkout changed during verifier reproduction; refusing publication" >&2
    exit 1
fi

PUBLISH_DIR="$(mktemp -d "$VERIFIER_DIR/.reproduce-publish.XXXXXX")"
for artifact in "${BINARIES[@]}" "${VKS[@]}" "${METADATA_FILES[@]}"; do
    cp -p -- "$STAGE_DIR/$artifact" "$PUBLISH_DIR/$artifact"
done
cp -p -- "$STAGE_DIR/reproduction-manifest.tsv" \
    "$PUBLISH_DIR/reproduction-manifest.tsv"

PUBLISH_BACKUP_DIR="$(mktemp -d "$VERIFIER_DIR/.artifact-backup.XXXXXX")"
for artifact in "${BINARIES[@]}" "${VKS[@]}" "${METADATA_FILES[@]}"; do
    if [[ -e "$VERIFIER_DIR/$artifact" ]]; then
        cp -p -- "$VERIFIER_DIR/$artifact" "$PUBLISH_BACKUP_DIR/$artifact"
    else
        : > "$PUBLISH_BACKUP_DIR/.absent-$artifact"
    fi
done
if [[ -e "$REPRODUCTION_MANIFEST_DESTINATION" ]]; then
    cp -p -- "$REPRODUCTION_MANIFEST_DESTINATION" \
        "$PUBLISH_BACKUP_DIR/reproduction-manifest.external"
else
    : > "$PUBLISH_BACKUP_DIR/.absent-reproduction-manifest.external"
fi
PUBLISH_STARTED=true

for artifact in "${BINARIES[@]}" "${VKS[@]}" "${METADATA_FILES[@]}"; do
    mv -f -- "$PUBLISH_DIR/$artifact" "$VERIFIER_DIR/$artifact"
done

# SYSCOIN: This per-run record includes the final source commit and Docker
# image ID, so it is intentionally outside the deterministic artifact commit.
# Publish it in the same rollback transaction as the canonical artifacts.
REPRODUCTION_MANIFEST_TMP=$(mktemp \
    "$REPRODUCTION_MANIFEST_DIRECTORY/.reproduction-manifest.XXXXXX")
cp -p -- "$PUBLISH_DIR/reproduction-manifest.tsv" "$REPRODUCTION_MANIFEST_TMP"
mv -f -- "$REPRODUCTION_MANIFEST_TMP" "$REPRODUCTION_MANIFEST_DESTINATION"
REPRODUCTION_MANIFEST_TMP=""
PUBLISH_COMMITTED=true

cat "$VERIFIER_DIR/artifact-manifest.tsv"
printf 'reproduction metadata: %s\n' "$REPRODUCTION_MANIFEST_DESTINATION"

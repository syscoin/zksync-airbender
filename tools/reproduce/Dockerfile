# SYSCOIN: Pin the architecture, base image, Debian archive, Rust toolchain,
# and cargo-binutils release used for canonical verifier/VK reproduction.
FROM --platform=linux/amd64 debian:bullseye-slim@sha256:4edd2740ebd41d71ef78eed0081c8979e287e2496c6fca892f5936d1e22adcd3 AS builder

ARG DEBIAN_SNAPSHOT=20250811T000000Z
ARG SOURCE_DATE_EPOCH=946684800
ARG RUSTUP_VERSION=1.28.2
ARG RUSTUP_INIT_SHA256=20a06e644b0d9bd2fbdbfd52d42540bdde820ea7df86e92e533c073da0cdd43c

ENV SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH} \
    CARGO_INCREMENTAL=0 \
    TZ=UTC \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8 \
    CARGO_HOME=/opt/cargo \
    RUSTUP_HOME=/opt/rustup
ENV PATH=/opt/cargo/bin:${PATH}

# SYSCOIN: Snapshot package indices prevent a later apt update from changing
# the canonical compiler's native dependencies.
RUN set -eux; \
    rm -f /etc/apt/sources.list.d/*; \
    printf 'Acquire::Check-Valid-Until "false";\n' > /etc/apt/apt.conf.d/99snapshot; \
    printf 'Acquire::http::No-Cache "true";\n' > /etc/apt/apt.conf.d/99no-cache; \
    printf 'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/%s bullseye main\n' "$DEBIAN_SNAPSHOT" > /etc/apt/sources.list; \
    printf 'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian-security/%s bullseye-security main\n' "$DEBIAN_SNAPSHOT" >> /etc/apt/sources.list; \
    apt-get -o Acquire::Check-Valid-Until=false update; \
    apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
        git \
        libssl-dev \
        pkg-config; \
    rm -rf /var/lib/apt/lists/*

# Version must match tools/verifier/rust-toolchain.toml.
RUN set -eux; \
    curl --proto '=https' --tlsv1.2 -sSf \
        "https://static.rust-lang.org/rustup/archive/${RUSTUP_VERSION}/x86_64-unknown-linux-gnu/rustup-init" \
        -o /tmp/rustup-init; \
    printf '%s  %s\n' "$RUSTUP_INIT_SHA256" /tmp/rustup-init | sha256sum --check; \
    chmod +x /tmp/rustup-init; \
    /tmp/rustup-init -y --profile minimal \
        --default-toolchain nightly-2025-07-25; \
    rm -f /tmp/rustup-init; \
    rustup target add riscv32i-unknown-none-elf; \
    rustup component add llvm-tools-preview rust-src; \
    cargo install cargo-binutils --version 0.4.0 --locked

COPY . /zksync-airbender
WORKDIR /zksync-airbender/tools/verifier

RUN ./build.sh

# SYSCOIN: Ship compiler and package provenance beside the bytecode hashes so
# a mainnet VK is never identified only by an image tag.
RUN set -eux; \
    target12_output="$(awk -F '\t' '$1 == "universal_no_delegation.bin" { print $1 }' build-profiles.tsv)"; \
    target12_profile="$(awk -F '\t' '$1 == "universal_no_delegation.bin" { print $2 }' build-profiles.tsv)"; \
    target12_features="$(awk -F '\t' '$1 == "universal_no_delegation.bin" { print $3 }' build-profiles.tsv)"; \
    target12_build_std="$(awk -F '\t' '$1 == "universal_no_delegation.bin" { print $4 }' build-profiles.tsv)"; \
    test "$target12_output" = universal_no_delegation.bin; \
    test "$target12_profile" = release-universal-no-delegation; \
    test "$target12_features" = universal_circuit_no_delegation,panic_output; \
    test "$target12_build_std" = plain; \
    { \
        printf 'base_image\t%s\n' 'debian:bullseye-slim@sha256:4edd2740ebd41d71ef78eed0081c8979e287e2496c6fca892f5936d1e22adcd3'; \
        printf 'debian_snapshot\t%s\n' "$DEBIAN_SNAPSHOT"; \
        printf 'source_date_epoch\t%s\n' "$SOURCE_DATE_EPOCH"; \
        printf 'rustup_init\t%s\t%s\n' "$RUSTUP_VERSION" "$RUSTUP_INIT_SHA256"; \
        printf 'cargo_binutils\t%s\n' '0.4.0'; \
        printf '\n[rustc]\n'; \
        rustc -Vv; \
        printf '\n[cargo]\n'; \
        cargo -V; \
        printf '\n[rustup]\n'; \
        rustup -V; \
        rustup show active-toolchain; \
        printf '\n[objcopy]\n'; \
        cargo objcopy --version; \
        printf '\n[locks]\n'; \
        sha256sum Cargo.lock ../../Cargo.lock Cargo.toml; \
        printf '\n[verifier-build-profiles]\n'; \
        cat build-profiles.tsv; \
        printf '\n[universal-no-delegation-profile]\n'; \
        awk 'BEGIN { emit = 0 } \
            /^\[profile\.release-universal-no-delegation\]$/ { emit = 1 } \
            emit && /^\[/ && $0 != "[profile.release-universal-no-delegation]" { exit } \
            emit { print }' Cargo.toml; \
        printf '\n[universal-no-delegation-normalized-command]\n'; \
        printf 'CARGO_TARGET_DIR=$VERIFIER_DIR/target/twelve cargo objcopy --locked --profile %s -Z build-std=core,panic_abort,alloc --features %s --no-default-features -- -O binary $STAGE_DIR/%s\n' \
            "$target12_profile" "$target12_features" "$target12_output"; \
        printf '\n[debian-packages]\n'; \
        dpkg-query -W -f='${binary:Package}\t${Version}\n' \
            > /tmp/debian-packages.tsv; \
        sort /tmp/debian-packages.tsv; \
        rm -f /tmp/debian-packages.tsv; \
    } > build-environment.txt; \
    environment_size="$(wc -c < build-environment.txt | tr -d '[:space:]')"; \
    environment_hash="$(sha256sum build-environment.txt | awk '{print $1}')"; \
    printf '%s  %s\n' "$environment_hash" build-environment.txt \
        >> artifact-manifest.sha256; \
    printf '%s\t%s\t%s\n' build-environment.txt "$environment_size" "$environment_hash" \
        >> artifact-manifest.tsv

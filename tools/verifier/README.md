# ZK verifier example.

Verifies the RISC-V circuit FRI proof within RISC-V.


This example is special - as verification requires to run on the `mini` machine, and has to be compiled in release mode.


It is also using special `+zimop` operations (flag in .cargo/config.toml)

Make sure to use profile cli (or --release) when compiling

```
cargo objcopy --profile cli  -- -O binary app.bin
```


Make sure to use machine `mini` when running / proving.
```
.... --machine mini 
```


It also uses 2MB of ROM (set in lds/memory.x), as the program is quite large (due to blake verifier) after the compilation.

## Verifier release artifacts

Verifier ROMs, VK JSON files, and their generated provenance are release
artifacts, not source files. Their exact per-file identities remain pinned in
the source file `execution_utils/verifier-artifacts.sha256`. A normal Cargo
build must receive both an absolute artifact directory and that manifest's
exact SHA-256:

```bash
export AIRBENDER_VERIFIER_ARTIFACT_DIR=/absolute/path/to/installed-artifacts
export AIRBENDER_VERIFIER_ARTIFACT_MANIFEST_SHA256=<64-lowercase-hex-digest>
cargo build --locked
```

Install a local or HTTPS release bundle only with both independently pinned
digests:

```bash
tools/verifier/install_release.sh \
  /absolute/path/to/verifier-artifacts.tar.gz \
  <archive-sha256> \
  <artifact-manifest-sha256> \
  /absolute/path/to/new-install-directory
```

The installer refuses an existing destination, unexpected archive members,
non-HTTPS remote URLs, any outer/inner checksum mismatch, or any manifest that
differs from the source-pinned file table. Use the two `export` commands it
prints for Cargo, including Cargo Git-dependency builds.

Release operators must first run the canonical Docker reproduction in
`tools/reproduce/reproduce.sh`; a direct `build.sh` intentionally lacks the
Docker build-environment record and cannot form a release. On Linux with GNU
tar, package the reproduced outputs deterministically with:

```bash
tools/verifier/package_release.sh \
  /absolute/path/to/verifier-artifacts.tar.gz
```

Publish the archive and its `.sha256` sidecar together, and record the source
commit, source tree, archive SHA-256, and artifact-manifest SHA-256 in the
release notes. `vk_generation_without_artifacts` is an internal bootstrap
feature used only by `build_vk.sh`; all other CLI builds embed the verified
release bundle.

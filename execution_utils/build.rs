use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const PINNED_MANIFEST: &[u8] = include_bytes!("verifier-artifacts.sha256");

const ARTIFACTS: &[&str] = &[
    "base_layer.bin",
    "recursion_layer.bin",
    "recursion_log_23_layer.bin",
    "recursion_layer_no_delegation.bin",
    "final_recursion_layer.bin",
    "base_layer_with_output.bin",
    "recursion_layer_with_output.bin",
    "recursion_log_23_layer_with_output.bin",
    "recursion_layer_no_delegation_with_output.bin",
    "final_recursion_layer_with_output.bin",
    "universal.bin",
    "universal_no_delegation.bin",
    "universal.reduced.vk.json",
    "universal.reduced_log23.vk.json",
    "universal_no_delegation.final.vk.json",
    "base_layer.reduced.vk.json",
    "recursion_layer.reduced.vk.json",
    "recursion_log_23_layer.reduced.vk.json",
    "recursion_layer_no_delegation.final.vk.json",
    "final_recursion_layer.final.vk.json",
    "build-profiles.tsv",
    "build-environment.txt",
];

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn require_sha256(value: &str, label: &str) {
    assert!(
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{label} must be a lowercase 64-character SHA-256"
    );
}

fn require_regular_file(path: &Path) -> Vec<u8> {
    let metadata = fs::symlink_metadata(path).unwrap_or_else(|error| {
        panic!(
            "missing verifier release artifact {}: {error}",
            path.display()
        )
    });
    assert!(
        metadata.file_type().is_file(),
        "verifier release artifact must be a regular non-symlink file: {}",
        path.display()
    );
    fs::read(path).unwrap_or_else(|error| {
        panic!(
            "failed to read verifier release artifact {}: {error}",
            path.display()
        )
    })
}

fn main() {
    println!("cargo:rerun-if-changed=verifier-artifacts.sha256");
    println!("cargo:rerun-if-env-changed=AIRBENDER_VERIFIER_ARTIFACT_DIR");
    println!("cargo:rerun-if-env-changed=AIRBENDER_VERIFIER_ARTIFACT_MANIFEST_SHA256");

    let release_artifacts = env::var_os("CARGO_FEATURE_RELEASE_VERIFIER_ARTIFACTS").is_some();
    let vk_bootstrap = env::var_os("CARGO_FEATURE_VK_GENERATION_WITHOUT_ARTIFACTS").is_some();
    assert!(
        release_artifacts ^ vk_bootstrap,
        "select exactly one execution_utils verifier artifact feature"
    );
    if vk_bootstrap {
        return;
    }

    let artifact_dir = PathBuf::from(
        env::var_os("AIRBENDER_VERIFIER_ARTIFACT_DIR").expect(
            "AIRBENDER_VERIFIER_ARTIFACT_DIR must name the absolute directory installed from the verifier release bundle",
        ),
    );
    assert!(
        artifact_dir.is_absolute(),
        "AIRBENDER_VERIFIER_ARTIFACT_DIR must be absolute"
    );
    let directory_metadata = fs::symlink_metadata(&artifact_dir).unwrap_or_else(|error| {
        panic!(
            "verifier release artifact directory does not exist {}: {error}",
            artifact_dir.display()
        )
    });
    assert!(
        directory_metadata.file_type().is_dir(),
        "verifier release artifact directory must be a non-symlink directory: {}",
        artifact_dir.display()
    );
    let artifact_dir = artifact_dir.canonicalize().unwrap_or_else(|error| {
        panic!(
            "failed to canonicalize verifier release artifact directory {}: {error}",
            artifact_dir.display()
        )
    });

    let expected_manifest_hash = env::var("AIRBENDER_VERIFIER_ARTIFACT_MANIFEST_SHA256")
        .expect("AIRBENDER_VERIFIER_ARTIFACT_MANIFEST_SHA256 must pin artifact-manifest.sha256");
    require_sha256(
        &expected_manifest_hash,
        "AIRBENDER_VERIFIER_ARTIFACT_MANIFEST_SHA256",
    );
    let pinned_manifest_hash = sha256(PINNED_MANIFEST);
    assert_eq!(
        expected_manifest_hash, pinned_manifest_hash,
        "caller-pinned manifest does not match execution_utils/verifier-artifacts.sha256"
    );

    let manifest_path = artifact_dir.join("artifact-manifest.sha256");
    let manifest = require_regular_file(&manifest_path);
    assert_eq!(
        manifest.as_slice(),
        PINNED_MANIFEST,
        "release manifest differs from execution_utils/verifier-artifacts.sha256"
    );
    let manifest_text =
        std::str::from_utf8(&manifest).expect("verifier artifact manifest must contain UTF-8 text");
    let lines: Vec<_> = manifest_text.lines().collect();
    assert_eq!(
        lines.len(),
        ARTIFACTS.len(),
        "verifier artifact manifest must contain the exact release file set"
    );

    for (line, expected_name) in lines.into_iter().zip(ARTIFACTS) {
        let (expected_hash, name) = line
            .split_once("  ")
            .expect("invalid artifact-manifest.sha256 record");
        require_sha256(expected_hash, "artifact manifest record");
        assert_eq!(
            name, *expected_name,
            "unexpected verifier artifact manifest order"
        );

        let artifact_path = artifact_dir.join(name);
        let artifact = require_regular_file(&artifact_path);
        assert_eq!(
            sha256(&artifact),
            expected_hash,
            "verifier release artifact hash mismatch: {name}"
        );
        println!("cargo:rerun-if-changed={}", artifact_path.display());
    }

    println!("cargo:rerun-if-changed={}", manifest_path.display());
    println!(
        "cargo:rustc-env=AIRBENDER_VERIFIER_ARTIFACT_DIR={}",
        artifact_dir.display()
    );
}

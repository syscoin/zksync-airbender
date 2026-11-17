# Replaying a verifier change against a known proof

The opt-in `streaming_equivalence` test compares two RV32 verifier builds against a shared native verification result. It exercises complete proofs and ordinary truncation by withholding the final oracle word. No proof blobs are stored in this repository.

Use the application binary and text that generated the artifacts, the recursive verifier directory of the artifact producer, and a separate candidate verifier directory. Source both verifier directories from immutable Git revisions with their checked-in binaries and setup snapshots. Each tested verifier must have distinct executable text. The [qualification receipt](syscoin-v2-streaming-qualification.json) records the guest, proof and verifier identities for the retained testnet comparison.

Export complete CLI `ProofArtifact` JSON files for the selected batches. Obtain the eight expected public-output words for each batch from the public batch statement or independently validated witness export. For a 32-byte public-input hash, interpret each successive four bytes as a little-endian u32. Obtain the expected eight program-chain words from the trusted application and producer's recursive images using `end_params --security-level 100`. Select the base, unrolled or unified chain that corresponds to the artifact's verification layer.

Set `AIRBENDER_EQUIVALENCE_CASE` to the path of a JSON manifest. Relative input paths resolve against the manifest's directory. This single-proof example shows the shape, using the public values of the retained batch-1 unified proof.

```json
{
  "app_bin": "app/multiblock_batch.bin",
  "app_text": "app/multiblock_batch.text",
  "baseline_verifiers": "baseline/tools/verifier",
  "candidate_verifiers": "candidate/tools/verifier",
  "security": "Security100",
  "max_cycles": 100000000,
  "cases": [{
    "expected_chain": [84413938, 3995000497, 3553705882, 423231651, 3210519057, 1543137115, 700562094, 4069119977],
    "inputs": [{
      "proof": "batch-1-unified.json",
      "batch_id": 1,
      "public_output": [842549234, 2200740716, 529057351, 3020387658, 58643548, 2701650347, 1224024561, 4272330828]
    }]
  }]
}
```

A single input may be a base, unrolled recursion, or unified recursion artifact. To exercise the combine opcode, put two or more consecutive unified batch artifacts and their public outputs in one case. The driver verifies each input separately and checks the native combined result against the ordered public-output hash and shared program chain. For each case it checks both application Keccaks, batch IDs and profile, then compares both successful exit PCs with the corresponding image's canonical exit, all sixteen registers with the trusted native result, and complete oracle consumption. For truncated inputs it checks that the next read requested the missing word before the successful exit PC.

From the candidate Airbender checkout, run the test with its pinned Rust toolchain.

```sh
AIRBENDER_EQUIVALENCE_CASE=/absolute/path/fixture.json cargo test --profile cli -p cli --test streaming_equivalence -- --ignored --nocapture
```

The pass/fail comparison is separate from benchmarks. Guest-cycle, RAM-access, runtime and GPU measurements in the receipt refer to the exact binaries and workloads named there.

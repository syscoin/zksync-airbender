The Security100 BLAKE2s delegation fixture is the first and only delegation proof
at CSR 0x7c7 in batch 1 of the pinned-memory minimal-range qualification.
Worker 2223c7aae24f5090b6830a6ab6ac0e315b3a132d produced the original
submission, final-2223c7a-pinned-fri/proof-1.json (SHA-256
d791201cd52051e2aaca582c93db393931b69999d46da94a18e0143336714591).
Decode its proof field as base64 and deserialize it as a standard bincode 2
UnrolledProgramProof. Take delegation_proofs[&0x7c7][0], call
verifier_common::proof_flattener::flatten_full_proof(proof, 0), and write the
resulting u32 words in little-endian order. The fixture contains 87 queries and
279545 oracle words. Its SHA-256 is
01874a26cfbc773acc708d721ec59ed6e7fa08c88f63d3a63e0165a9f5992770.

#![cfg_attr(not(test), no_std)]

// SYSCOIN: Consensus-shared SHA-256 round primitive used by the 0x7cb delegation.

pub const SYSCOIN_SHA256_ROUND_FUNCTION_ACCESS_ID: u32 = 0x7cb;
pub const SHA256_STATE_WORDS: usize = 8;

pub const IV: [u32; SHA256_STATE_WORDS] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

pub const ROUND_CONSTANTS: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

#[inline(always)]
pub const fn small_sigma0(x: u32) -> u32 {
    x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3)
}

#[inline(always)]
pub const fn small_sigma1(x: u32) -> u32 {
    x.rotate_right(17) ^ x.rotate_right(19) ^ (x >> 10)
}

#[inline(always)]
pub const fn big_sigma0(x: u32) -> u32 {
    x.rotate_right(2) ^ x.rotate_right(13) ^ x.rotate_right(22)
}

#[inline(always)]
pub const fn big_sigma1(x: u32) -> u32 {
    x.rotate_right(6) ^ x.rotate_right(11) ^ x.rotate_right(25)
}

#[inline(always)]
pub const fn choose(x: u32, y: u32, z: u32) -> u32 {
    z ^ (x & (y ^ z))
}

#[inline(always)]
pub const fn majority(x: u32, y: u32, z: u32) -> u32 {
    (x & y) ^ (x & z) ^ (y & z)
}

/// Applies one delegated SHA-256 round to `(a, b, c, d, e, f, g, h)`.
#[inline]
pub fn delegated_round(state: &mut [u32; SHA256_STATE_WORDS], w: u32, k: u32) {
    let [a, b, c, d, e, f, g, h] = *state;
    let t1 = h
        .wrapping_add(big_sigma1(e))
        .wrapping_add(choose(e, f, g))
        .wrapping_add(k)
        .wrapping_add(w);
    let t2 = big_sigma0(a).wrapping_add(majority(a, b, c));

    *state = [t1.wrapping_add(t2), a, b, c, d.wrapping_add(t1), e, f, g];
}

/// Reference compression using the same 64-call state transition as the delegated guest path.
pub fn compress(state: &mut [u32; SHA256_STATE_WORDS], block: &[u32; 16]) {
    let initial = *state;
    let mut working = *state;
    let mut schedule = [0u32; 64];
    schedule[..16].copy_from_slice(block);
    for round in 16..64 {
        schedule[round] = small_sigma1(schedule[round - 2])
            .wrapping_add(schedule[round - 7])
            .wrapping_add(small_sigma0(schedule[round - 15]))
            .wrapping_add(schedule[round - 16]);
    }
    for (round, &k) in ROUND_CONSTANTS.iter().enumerate() {
        delegated_round(&mut working, schedule[round], k);
    }
    for (dst, (&old, &working)) in state.iter_mut().zip(initial.iter().zip(working.iter())) {
        *dst = old.wrapping_add(working);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::compress256;
    use sha2::digest::generic_array::GenericArray;

    fn reference(state: &mut [u32; 8], block: &[u32; 16]) {
        let mut bytes = [0u8; 64];
        for (dst, word) in bytes.chunks_exact_mut(4).zip(block) {
            dst.copy_from_slice(&word.to_be_bytes());
        }
        compress256(state, &[GenericArray::clone_from_slice(&bytes)]);
    }

    #[test]
    fn delegated_compression_matches_rustcrypto_known_and_varied_inputs() {
        let mut cases = vec![([0u32; 8], [0u32; 16]), (IV, [0u32; 16])];
        let mut block = [0u32; 16];
        for (i, word) in block.iter_mut().enumerate() {
            *word = (i as u32).wrapping_mul(0x9e37_79b9) ^ 0xa5a5_5a5a;
        }
        cases.push(([0xffff_ffff; 8], block));

        let mut seed = 0x6a09_e667_f3bc_c908u64;
        for _ in 0..128 {
            let mut state = [0u32; 8];
            let mut block = [0u32; 16];
            for word in state.iter_mut().chain(block.iter_mut()) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                *word = seed as u32;
            }
            cases.push((state, block));
        }

        for (state, block) in cases {
            let mut got = state;
            let mut expected = state;
            compress(&mut got, &block);
            reference(&mut expected, &block);
            assert_eq!(got, expected);
        }
    }
}

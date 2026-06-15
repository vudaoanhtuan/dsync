//! Thin wrapper over the `fast_rsync` crate: signature / diff / apply, plus the block-size
//! heuristic. We own only the orchestration around the library. See specs/delta-algorithm.md.

use fast_rsync::{apply as fr_apply, diff as fr_diff, Signature as FrSignature, SignatureOptions};
use serde::{Deserialize, Serialize};

use crate::error::{DsyncError, Result};

/// fast_rsync's internal MD4 strong-hash truncation (NOT our integrity hash). 8 bytes is a
/// good speed/robustness tradeoff; BLAKE3 is the real correctness gate (see the engine).
const CRYPTO_HASH_SIZE: u32 = 8;

/// Opaque serialized signature blob (fast_rsync's bytes), shaped for the wire (spec 6).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Signature(pub Vec<u8>);

/// Opaque serialized delta blob (fast_rsync's bytes).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Delta(pub Vec<u8>);

/// Block-size heuristic, rsync-like: nearest power of two of sqrt(len) (ties round up),
/// clamped to [1024, 131072]. e.g. len=1_000_000 → sqrt≈1000 → 1024.
pub fn block_size_for(file_len: u64) -> u32 {
    let sqrt = (file_len as f64).sqrt();
    let pow2 = round_pow2(sqrt);
    pow2.clamp(1024, 131_072)
}

/// Nearest power of two to `x` (ties → up).
fn round_pow2(x: f64) -> u32 {
    if x <= 1.0 {
        return 1;
    }
    let log = x.log2();
    let candidate = log.round(); // .round() rounds half away from zero → ties up
    2f64.powf(candidate) as u32
}

/// Build a signature for the basis bytes.
pub fn signature(basis: &[u8]) -> Result<Signature> {
    let opts = SignatureOptions {
        block_size: block_size_for(basis.len() as u64),
        crypto_hash_size: CRYPTO_HASH_SIZE,
    };
    let sig = FrSignature::calculate(basis, opts);
    Ok(Signature(sig.into_serialized()))
}

/// Produce a delta turning `basis` (described by `sig`) into `new_data`.
pub fn diff(sig: &Signature, new_data: &[u8]) -> Result<Delta> {
    let parsed = FrSignature::deserialize(sig.0.clone())
        .map_err(|e| DsyncError::Other(format!("invalid signature: {e}")))?;
    let indexed = parsed.index();
    let mut out = Vec::new();
    fr_diff(&indexed, new_data, &mut out)
        .map_err(|e| DsyncError::Other(format!("delta diff failed: {e}")))?;
    Ok(Delta(out))
}

/// Apply a delta to `basis`, returning the reconstructed new file bytes.
pub fn apply(basis: &[u8], delta: &Delta) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    fr_apply(basis, &delta.0, &mut out)
        .map_err(|e| DsyncError::Other(format!("delta apply failed: {e}")))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, RngCore, SeedableRng};
    use rand::rngs::StdRng;

    fn roundtrip(basis: &[u8], new: &[u8]) {
        let sig = signature(basis).unwrap();
        let delta = diff(&sig, new).unwrap();
        let out = apply(basis, &delta).unwrap();
        assert_eq!(out, new, "roundtrip mismatch");
    }

    #[test]
    fn block_size_examples() {
        assert_eq!(block_size_for(1_000_000), 1024);
        assert_eq!(block_size_for(0), 1024); // clamped up
        assert_eq!(block_size_for(u64::MAX), 131_072); // clamped down
    }

    #[test]
    fn identical_files() {
        let mut rng = StdRng::seed_from_u64(1);
        let mut data = vec![0u8; 50_000];
        rng.fill_bytes(&mut data);
        roundtrip(&data, &data);
        // identical → delta should be far smaller than the file
        let sig = signature(&data).unwrap();
        let delta = diff(&sig, &data).unwrap();
        assert!(delta.0.len() < data.len() / 4, "delta not small for identical files");
    }

    #[test]
    fn fully_different() {
        let mut rng = StdRng::seed_from_u64(2);
        let mut a = vec![0u8; 20_000];
        let mut b = vec![0u8; 20_000];
        rng.fill_bytes(&mut a);
        rng.fill_bytes(&mut b);
        roundtrip(&a, &b);
    }

    #[test]
    fn empty_cases() {
        roundtrip(&[], &[]);
        roundtrip(&[], b"hello world");
        roundtrip(b"hello world", &[]);
    }

    #[test]
    fn smaller_than_block() {
        roundtrip(b"abc", b"abcd");
    }

    #[test]
    fn single_byte_changes() {
        let mut rng = StdRng::seed_from_u64(3);
        let mut base = vec![0u8; 30_000];
        rng.fill_bytes(&mut base);
        for pos in [0usize, 15_000, 29_999] {
            let mut new = base.clone();
            new[pos] ^= 0xFF;
            roundtrip(&base, &new);
        }
    }

    #[test]
    fn insert_and_delete() {
        let mut rng = StdRng::seed_from_u64(4);
        let mut base = vec![0u8; 40_000];
        rng.fill_bytes(&mut base);
        // insertion at middle
        let mut ins = base.clone();
        ins.splice(20_000..20_000, b"INSERTED PAYLOAD".iter().cloned());
        roundtrip(&base, &ins);
        // deletion
        let mut del = base.clone();
        del.drain(10_000..10_500);
        roundtrip(&base, &del);
    }

    #[test]
    fn large_file_small_change_yields_small_delta() {
        let mut rng = StdRng::seed_from_u64(5);
        let mut base = vec![0u8; 4_000_000];
        rng.fill_bytes(&mut base);
        let mut new = base.clone();
        for i in 0..16 {
            new[1_000_000 + i] = rng.gen();
        }
        let sig = signature(&base).unwrap();
        let delta = diff(&sig, &new).unwrap();
        let out = apply(&base, &delta).unwrap();
        assert_eq!(out, new);
        assert!(
            delta.0.len() < base.len() / 10,
            "expected delta ≪ file, got {} of {}",
            delta.0.len(),
            base.len()
        );
    }
}

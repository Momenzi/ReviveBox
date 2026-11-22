//! Cryptographically verified `bsdiff` binary patching.

use std::io::Cursor;

use ogsc_scanner::sha256_bytes;

use crate::error::PatcherError;

/// Verifies `original_bytes` SHA-256, applies `patch_bytes` via `bsdiff`, and verifies `expected_patched_sha256`
/// before returning the patched payload.
pub fn apply_verified_bsdiff(
    original_bytes: &[u8],
    patch_bytes: &[u8],
    target_file: &str,
    expected_original_sha256: &str,
    expected_patched_sha256: &str,
) -> Result<Vec<u8>, PatcherError> {
    let expected_orig = expected_original_sha256.to_ascii_lowercase();
    let expected_patched = expected_patched_sha256.to_ascii_lowercase();

    let actual_orig = sha256_bytes(original_bytes);
    if actual_orig != expected_orig {
        return Err(PatcherError::HashMismatch {
            subject: format!("{target_file} (pre-patch original)"),
            expected: expected_orig,
            actual: actual_orig,
        });
    }

    let mut patched = Vec::new();
    let mut cursor = Cursor::new(patch_bytes);
    bsdiff::patch(original_bytes, &mut cursor, &mut patched).map_err(|err| {
        PatcherError::BsdiffError { target_file: target_file.to_owned(), reason: err.to_string() }
    })?;

    let actual_patched = sha256_bytes(&patched);
    if actual_patched != expected_patched {
        return Err(PatcherError::HashMismatch {
            subject: format!("{target_file} (post-patch verification)"),
            expected: expected_patched,
            actual: actual_patched,
        });
    }

    Ok(patched)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_original_applies_bsdiff_and_verifies_patched_hash() {
        let original = b"LEGACY_GAME_BINARY_V1_0000000000000000";
        let expected_new = b"LEGACY_GAME_BINARY_V1_1_WIDESCREEN_FIX";

        let mut patch_bytes = Vec::new();
        bsdiff::diff(original, expected_new, &mut patch_bytes).expect("create bsdiff");

        let orig_sha = sha256_bytes(original);
        let new_sha = sha256_bytes(expected_new);

        let patched =
            apply_verified_bsdiff(original, &patch_bytes, "game.dll", &orig_sha, &new_sha)
                .expect("apply valid bsdiff");
        assert_eq!(patched, expected_new);

        // Wrong pre-patch hash aborts before patching
        let bad_orig =
            apply_verified_bsdiff(b"WRONG_BINARY", &patch_bytes, "game.dll", &orig_sha, &new_sha)
                .expect_err("wrong pre-hash must fail");
        assert!(matches!(bad_orig, PatcherError::HashMismatch { .. }));

        // Wrong expected post-patch hash aborts before returning
        let wrong_post_hash = std::iter::repeat_n('f', 64).collect::<String>();
        let bad_post =
            apply_verified_bsdiff(original, &patch_bytes, "game.dll", &orig_sha, &wrong_post_hash)
                .expect_err("wrong post-hash must fail");
        assert!(matches!(bad_post, PatcherError::HashMismatch { .. }));
    }
}

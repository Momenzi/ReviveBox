//! Buffered streaming cryptographic hashing (SHA-256, partial SHA-256, and MD5) for executable identification.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use md5::Md5;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::ScannerError;

const IO_BUFFER_SIZE: usize = 64 * 1024;

/// Default byte window (`64 KiB`) used when generating partial executable fingerprints.
pub const DEFAULT_PARTIAL_HASH_BYTES: u64 = 64 * 1024;

/// Full cryptographic digests and byte length computed for a local executable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputedHashes {
    /// Exact file length in bytes.
    pub file_size_bytes: u64,
    /// Lowercase hexadecimal SHA-256 digest of the entire file.
    pub sha256: String,
    /// Lowercase hexadecimal MD5 digest of the entire file.
    pub md5: String,
}

/// Complete fingerprint bundle suitable for populating a recipe's `ExecutableFingerprint`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratedFingerprint {
    /// Exact file length in bytes.
    pub file_size_bytes: u64,
    /// Byte offset where the partial SHA-256 slice begins (always `0`).
    pub partial_offset: u64,
    /// Number of bytes included in `partial_sha256` (`min(file_size_bytes, requested_length)`).
    pub partial_length: u64,
    /// Lowercase hexadecimal SHA-256 of `[0..partial_length]`.
    pub partial_sha256: String,
    /// Lowercase hexadecimal full-file SHA-256 digest.
    pub sha256: String,
    /// Lowercase hexadecimal full-file MD5 digest.
    pub md5: String,
}

/// Computes the lowercase hex SHA-256 digest of an in-memory byte slice.
pub fn sha256_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Computes the full lowercase hex SHA-256 digest of `path` using buffered 64 KiB reads.
pub fn compute_sha256(path: &Path) -> Result<String, ScannerError> {
    let file =
        File::open(path).map_err(|source| ScannerError::Io { path: path.to_path_buf(), source })?;
    let mut reader = BufReader::with_capacity(IO_BUFFER_SIZE, file);
    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; IO_BUFFER_SIZE];

    loop {
        let read_bytes = reader
            .read(&mut buf)
            .map_err(|source| ScannerError::Io { path: path.to_path_buf(), source })?;
        if read_bytes == 0 {
            break;
        }
        hasher.update(&buf[..read_bytes]);
    }

    Ok(hex::encode(hasher.finalize()))
}

/// Computes the lowercase hex SHA-256 digest of up to `length` bytes starting at `offset`.
pub fn compute_partial_sha256(
    path: &Path,
    offset: u64,
    length: u64,
) -> Result<String, ScannerError> {
    let mut file =
        File::open(path).map_err(|source| ScannerError::Io { path: path.to_path_buf(), source })?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|source| ScannerError::Io { path: path.to_path_buf(), source })?;

    let mut hasher = Sha256::new();
    let mut buf = vec![0_u8; IO_BUFFER_SIZE];
    let mut remaining = length;

    while remaining > 0 {
        #[allow(clippy::cast_possible_truncation)]
        let max_to_read = (remaining.min(buf.len() as u64)) as usize;
        let read_bytes = file
            .read(&mut buf[..max_to_read])
            .map_err(|source| ScannerError::Io { path: path.to_path_buf(), source })?;
        if read_bytes == 0 {
            break;
        }
        hasher.update(&buf[..read_bytes]);
        remaining -= read_bytes as u64;
    }

    Ok(hex::encode(hasher.finalize()))
}

/// Computes file size, full SHA-256, and full MD5 in a single buffered streaming pass.
pub fn compute_hashes(path: &Path) -> Result<ComputedHashes, ScannerError> {
    let file =
        File::open(path).map_err(|source| ScannerError::Io { path: path.to_path_buf(), source })?;
    let mut reader = BufReader::with_capacity(IO_BUFFER_SIZE, file);
    let mut sha_hasher = Sha256::new();
    let mut md5_hasher = Md5::new();
    let mut buf = vec![0_u8; IO_BUFFER_SIZE];
    let mut total_bytes = 0_u64;

    loop {
        let read_bytes = reader
            .read(&mut buf)
            .map_err(|source| ScannerError::Io { path: path.to_path_buf(), source })?;
        if read_bytes == 0 {
            break;
        }
        let chunk = &buf[..read_bytes];
        sha_hasher.update(chunk);
        md5_hasher.update(chunk);
        total_bytes += read_bytes as u64;
    }

    Ok(ComputedHashes {
        file_size_bytes: total_bytes,
        sha256: hex::encode(sha_hasher.finalize()),
        md5: hex::encode(md5_hasher.finalize()),
    })
}

/// Computes size, partial head SHA-256 (`0..partial_length`), full SHA-256, and MD5 in a single read pass.
pub fn compute_fingerprint(
    path: &Path,
    partial_length: u64,
) -> Result<GeneratedFingerprint, ScannerError> {
    let file =
        File::open(path).map_err(|source| ScannerError::Io { path: path.to_path_buf(), source })?;
    let mut reader = BufReader::with_capacity(IO_BUFFER_SIZE, file);
    let mut partial_hasher = Sha256::new();
    let mut full_sha_hasher = Sha256::new();
    let mut md5_hasher = Md5::new();
    let mut buf = vec![0_u8; IO_BUFFER_SIZE];
    let mut total_bytes = 0_u64;
    let mut partial_remaining = partial_length;
    let mut actual_partial_len = 0_u64;

    loop {
        let read_bytes = reader
            .read(&mut buf)
            .map_err(|source| ScannerError::Io { path: path.to_path_buf(), source })?;
        if read_bytes == 0 {
            break;
        }
        let chunk = &buf[..read_bytes];
        full_sha_hasher.update(chunk);
        md5_hasher.update(chunk);

        if partial_remaining > 0 {
            #[allow(clippy::cast_possible_truncation)]
            let take = (partial_remaining.min(read_bytes as u64)) as usize;
            partial_hasher.update(&chunk[..take]);
            partial_remaining -= take as u64;
            actual_partial_len += take as u64;
        }

        total_bytes += read_bytes as u64;
    }

    Ok(GeneratedFingerprint {
        file_size_bytes: total_bytes,
        partial_offset: 0,
        partial_length: actual_partial_len,
        partial_sha256: hex::encode(partial_hasher.finalize()),
        sha256: hex::encode(full_sha_hasher.finalize()),
        md5: hex::encode(md5_hasher.finalize()),
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn matches_known_sha256_and_md5_vectors() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("abc.bin");
        fs::write(&file_path, b"abc").expect("write test file");

        let hashes = compute_hashes(&file_path).expect("hash abc");
        assert_eq!(hashes.file_size_bytes, 3);
        assert_eq!(
            hashes.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(hashes.md5, "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(compute_sha256(&file_path).expect("sha256"), hashes.sha256);
    }

    #[test]
    fn streams_multi_chunk_files_and_partial_slices_accurately() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let file_path = dir.path().join("large.bin");
        let mut payload = vec![0x42_u8; 100_000];
        payload[65_536..65_540].copy_from_slice(b"OGSC");
        fs::write(&file_path, &payload).expect("write multi-chunk payload");

        let fp = compute_fingerprint(&file_path, 65_536).expect("fingerprint");
        assert_eq!(fp.file_size_bytes, 100_000);
        assert_eq!(fp.partial_length, 65_536);

        let partial = compute_partial_sha256(&file_path, 0, 65_536).expect("partial head");
        assert_eq!(fp.partial_sha256, partial);

        let tail_slice = compute_partial_sha256(&file_path, 65_536, 4).expect("partial tail");
        assert_eq!(tail_slice, sha256_bytes(b"OGSC"));
    }
}

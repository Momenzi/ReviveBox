//! Streaming HTTPS downloader with SHA-256 verification and content-addressed local cache.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ogsc_scanner::compute_sha256;
use url::Url;

use crate::error::PatcherError;
use crate::events::{EventSink, PatchEvent};

const DOWNLOAD_CHUNK_SIZE: usize = 64 * 1024;

/// Abstraction over HTTPS file downloads enabling hermetic tests via local/mock downloaders.
pub trait Downloader {
    /// Streams `url` into `dest`, invoking `on_progress(bytes_downloaded, total_bytes)` incrementally.
    fn download_to_file(
        &self,
        url: &str,
        dest: &Path,
        on_progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<(), PatcherError>;
}

/// Production HTTPS downloader powered by `ureq` with TLS verification and streaming progress.
#[derive(Debug, Clone)]
pub struct HttpDownloader {
    agent: ureq::Agent,
}

impl Default for HttpDownloader {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpDownloader {
    /// Constructs a new [`HttpDownloader`] with sensible connect and read timeouts.
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(15))
            .timeout_read(Duration::from_secs(120))
            .build();
        Self { agent }
    }

    /// Returns the default local cache directory for verified downloaded archives and patches.
    pub fn default_cache_dir() -> PathBuf {
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local_app_data).join("OldGamesSecondChance").join("cache");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".cache").join("old-games-second-chance");
        }
        PathBuf::from(".ogsc_cache")
    }
}

impl Downloader for HttpDownloader {
    fn download_to_file(
        &self,
        url: &str,
        dest: &Path,
        on_progress: &mut dyn FnMut(u64, Option<u64>),
    ) -> Result<(), PatcherError> {
        ensure_https_url(url)?;

        let response = self
            .agent
            .get(url)
            .set("User-Agent", concat!("OldGamesSecondChance/", env!("CARGO_PKG_VERSION")))
            .call()
            .map_err(|err| PatcherError::DownloadFailed {
                url: url.to_owned(),
                reason: err.to_string(),
            })?;

        let total_bytes =
            response.header("Content-Length").and_then(|val| val.trim().parse::<u64>().ok());
        let mut reader = response.into_reader();

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|source| PatcherError::Io { path: parent.to_path_buf(), source })?;
        }

        let mut out = File::create(dest)
            .map_err(|source| PatcherError::Io { path: dest.to_path_buf(), source })?;
        let mut buf = vec![0_u8; DOWNLOAD_CHUNK_SIZE];
        let mut downloaded = 0_u64;

        loop {
            let read_bytes = reader.read(&mut buf).map_err(|err| PatcherError::DownloadFailed {
                url: url.to_owned(),
                reason: format!("stream read error: {err}"),
            })?;
            if read_bytes == 0 {
                break;
            }
            out.write_all(&buf[..read_bytes])
                .map_err(|source| PatcherError::Io { path: dest.to_path_buf(), source })?;
            downloaded += read_bytes as u64;
            on_progress(downloaded, total_bytes);
        }

        out.sync_all().map_err(|source| PatcherError::Io { path: dest.to_path_buf(), source })?;
        Ok(())
    }
}

/// Fetches `primary_url` (falling back to `mirror_urls`) into `<cache_dir>/<expected_sha256>`
/// or reuses an existing SHA-256-verified cached copy.
pub fn fetch_verified_file(
    downloader: &dyn Downloader,
    cache_dir: &Path,
    primary_url: &str,
    mirror_urls: &[String],
    expected_sha256: &str,
    step_id: &str,
    sink: &mut dyn EventSink,
) -> Result<PathBuf, PatcherError> {
    fs::create_dir_all(cache_dir)
        .map_err(|source| PatcherError::Io { path: cache_dir.to_path_buf(), source })?;

    let expected_lower = expected_sha256.to_ascii_lowercase();
    let cached_path = cache_dir.join(&expected_lower);

    if cached_path.is_file() {
        let actual = compute_sha256(&cached_path)?;
        if actual == expected_lower {
            let bytes = fs::metadata(&cached_path)
                .map_err(|source| PatcherError::Io { path: cached_path.clone(), source })?
                .len();
            sink.emit(PatchEvent::DownloadStarted {
                step_id: step_id.to_owned(),
                url: primary_url.to_owned(),
                from_cache: true,
            });
            sink.emit(PatchEvent::DownloadVerified {
                step_id: step_id.to_owned(),
                sha256: expected_lower,
                bytes,
            });
            return Ok(cached_path);
        }
        let _ = fs::remove_file(&cached_path);
    }

    let mut urls = Vec::with_capacity(1 + mirror_urls.len());
    urls.push(primary_url);
    for m in mirror_urls {
        urls.push(m.as_str());
    }

    let mut last_error: Option<PatcherError> = None;

    for url in urls {
        ensure_https_url(url)?;
        sink.emit(PatchEvent::DownloadStarted {
            step_id: step_id.to_owned(),
            url: url.to_owned(),
            from_cache: false,
        });

        let part_path = cache_dir.join(format!("{expected_lower}.{}.part", std::process::id()));
        let mut progress_cb = |downloaded: u64, total: Option<u64>| {
            sink.emit(PatchEvent::DownloadProgress {
                step_id: step_id.to_owned(),
                bytes_downloaded: downloaded,
                total_bytes: total,
            });
        };

        if let Err(err) = downloader.download_to_file(url, &part_path, &mut progress_cb) {
            let _ = fs::remove_file(&part_path);
            sink.emit(PatchEvent::Warning {
                message: format!("download attempt failed for `{url}`: {err}"),
            });
            last_error = Some(err);
            continue;
        }

        let actual_sha = compute_sha256(&part_path)?;
        if actual_sha != expected_lower {
            let _ = fs::remove_file(&part_path);
            let err = PatcherError::HashMismatch {
                subject: url.to_owned(),
                expected: expected_lower.clone(),
                actual: actual_sha,
            };
            last_error = Some(err);
            continue;
        }

        let bytes = fs::metadata(&part_path)
            .map_err(|source| PatcherError::Io { path: part_path.clone(), source })?
            .len();

        fs::rename(&part_path, &cached_path)
            .map_err(|source| PatcherError::Io { path: cached_path.clone(), source })?;

        sink.emit(PatchEvent::DownloadVerified {
            step_id: step_id.to_owned(),
            sha256: expected_lower,
            bytes,
        });
        return Ok(cached_path);
    }

    Err(last_error.unwrap_or_else(|| PatcherError::DownloadFailed {
        url: primary_url.to_owned(),
        reason: "no download URLs provided".to_owned(),
    }))
}

fn ensure_https_url(raw: &str) -> Result<(), PatcherError> {
    let parsed = Url::parse(raw).map_err(|err| PatcherError::DownloadFailed {
        url: raw.to_owned(),
        reason: format!("invalid URL: {err}"),
    })?;
    if parsed.scheme() != "https" || !parsed.has_host() {
        return Err(PatcherError::DownloadFailed {
            url: raw.to_owned(),
            reason: "only https:// URLs with a valid host are permitted".to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ogsc_scanner::sha256_bytes;

    use super::*;
    use crate::events::VecEventSink;

    struct FakeDownloader {
        files: BTreeMap<String, Vec<u8>>,
    }

    impl Downloader for FakeDownloader {
        fn download_to_file(
            &self,
            url: &str,
            dest: &Path,
            on_progress: &mut dyn FnMut(u64, Option<u64>),
        ) -> Result<(), PatcherError> {
            let bytes = self.files.get(url).ok_or_else(|| PatcherError::DownloadFailed {
                url: url.to_owned(),
                reason: "404 mock not found".to_owned(),
            })?;
            fs::write(dest, bytes)
                .map_err(|source| PatcherError::Io { path: dest.to_path_buf(), source })?;
            on_progress(bytes.len() as u64, Some(bytes.len() as u64));
            Ok(())
        }
    }

    #[test]
    fn rejects_non_https_and_hash_mismatches_and_reuses_verified_cache() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let cache = dir.path().join("cache");
        let payload = b"VERIFIED_COMMUNITY_FIX_ARCHIVE";
        let digest = sha256_bytes(payload);

        let dl = FakeDownloader {
            files: BTreeMap::from([(
                "https://mirror.example.org/fix.zip".to_owned(),
                payload.to_vec(),
            )]),
        };
        let mut sink = VecEventSink::default();

        // Reject http:// outright
        let http_err = fetch_verified_file(
            &dl,
            &cache,
            "http://insecure.example.org/fix.zip",
            &[],
            &digest,
            "step-1",
            &mut sink,
        )
        .expect_err("http must be rejected");
        assert!(matches!(http_err, PatcherError::DownloadFailed { .. }));

        // Primary 404 falls back to verified mirror
        let cached = fetch_verified_file(
            &dl,
            &cache,
            "https://primary.example.org/404.zip",
            &["https://mirror.example.org/fix.zip".to_owned()],
            &digest,
            "step-1",
            &mut sink,
        )
        .expect("mirror fallback should succeed");
        assert_eq!(fs::read(&cached).expect("read cached"), payload);

        // Wrong digest never caches or succeeds
        let wrong_digest = std::iter::repeat_n('0', 64).collect::<String>();
        let hash_err = fetch_verified_file(
            &dl,
            &cache,
            "https://mirror.example.org/fix.zip",
            &[],
            &wrong_digest,
            "step-2",
            &mut sink,
        )
        .expect_err("hash mismatch must fail");
        assert!(matches!(hash_err, PatcherError::HashMismatch { .. }));
        assert!(!cache.join(&wrong_digest).exists());

        // Second fetch of valid digest hits cache without calling downloader
        let empty_dl = FakeDownloader { files: BTreeMap::new() };
        let mut hit_sink = VecEventSink::default();
        let hit = fetch_verified_file(
            &empty_dl,
            &cache,
            "https://mirror.example.org/fix.zip",
            &[],
            &digest,
            "step-3",
            &mut hit_sink,
        )
        .expect("cache hit should avoid network");
        assert_eq!(hit, cached);
        assert!(matches!(
            hit_sink.events.first(),
            Some(PatchEvent::DownloadStarted { from_cache: true, .. })
        ));
    }
}

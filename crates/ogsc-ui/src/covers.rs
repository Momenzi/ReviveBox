//! Asynchronous SteamGridDB cover art downloader and disk cache with clean offline fallback.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use image::imageops::FilterType;
use image::{GenericImageView, ImageFormat};
use serde::Deserialize;
use thiserror::Error;
use url::Url;

use crate::settings::write_bytes_atomic;

const COVER_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp"];
const MAX_COVER_HEIGHT: u32 = 600;
const MAX_COVER_WIDTH: u32 = 400;
const COVER_USER_AGENT: &str =
    concat!("ReviveBox/", env!("CARGO_PKG_VERSION"), " (+https://github.com/Momenzi/ReviveBox)");

/// Parameters describing how to locate or fetch cover artwork for one library entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverRequest {
    /// Stable game identifier used for on-disk cache naming.
    pub game_id: String,
    /// Human-readable title used if SteamGridDB autocomplete lookup is required.
    pub title: String,
    /// Optional explicit SteamGridDB game ID from `RecipeMetadata::steamgriddb_id`.
    pub steamgriddb_id: Option<u64>,
    /// Optional Steam App ID from `RecipeMetadata::steam_app_id`.
    pub steam_app_id: Option<u32>,
    /// Optional direct HTTPS cover artwork URL from `RecipeMetadata::cover_art_url`.
    pub cover_art_url: Option<String>,
    /// Optional explicit cover image path stored on the library entry.
    pub custom_cover_art_path: Option<PathBuf>,
    /// Local cover cache directory (`AppSettings::covers_dir`).
    pub covers_dir: PathBuf,
    /// Optional SteamGridDB v2 Bearer token.
    pub api_key: String,
}

/// Decoded RGBA8 cover image ready for GPU texture upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedCover {
    /// Image width in pixels.
    pub width: usize,
    /// Image height in pixels.
    pub height: usize,
    /// Unmultiplied RGBA pixel buffer (`width * height * 4` bytes).
    pub rgba: Vec<u8>,
}

/// Result of attempting to load cached or SteamGridDB artwork for a game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoverLoadOutcome {
    /// Artwork was loaded and decoded cleanly.
    Loaded {
        /// Owning recipe `game_id`.
        game_id: String,
        /// Decoded RGBA pixel buffer.
        cover: DecodedCover,
        /// `true` when loaded from local custom path or disk cache without network calls.
        from_disk_cache: bool,
    },
    /// No local or remote artwork could be loaded; render deterministic fallback art.
    Fallback {
        /// Owning recipe `game_id`.
        game_id: String,
        /// Optional diagnostic note explaining why remote fetch was skipped or failed.
        reason: Option<String>,
    },
}

/// Errors returned by [`CoverHttpClient`] implementations.
#[derive(Debug, Error)]
pub enum CoverError {
    /// HTTP/TLS transport failure or non-HTTPS URL.
    #[error("cover request to `{url}` failed: {reason}")]
    Http {
        /// Request URL.
        url: String,
        /// Failure details.
        reason: String,
    },
}

/// Minimal HTTP client abstraction for SteamGridDB cover art queries, mockable in unit tests.
pub trait CoverHttpClient {
    /// Sends an authenticated GET request (`Authorization: Bearer <token>`) and returns UTF-8 JSON.
    fn get_json(&self, url: &str, bearer_token: &str) -> Result<String, CoverError>;

    /// Downloads raw image bytes from an `https://` CDN URL.
    fn get_bytes(&self, url: &str) -> Result<Vec<u8>, CoverError>;
}

/// Production HTTP client backed by `ureq`.
#[derive(Debug, Clone)]
pub struct UreqCoverClient {
    agent: ureq::Agent,
}

impl Default for UreqCoverClient {
    fn default() -> Self {
        Self::new()
    }
}

impl UreqCoverClient {
    /// Builds a [`UreqCoverClient`] with short timeouts appropriate for optional UI artwork.
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(8))
            .timeout_read(Duration::from_secs(15))
            .build();
        Self { agent }
    }
}

impl CoverHttpClient for UreqCoverClient {
    fn get_json(&self, url: &str, bearer_token: &str) -> Result<String, CoverError> {
        let _ = ensure_https(url)?;
        let response = self
            .agent
            .get(url)
            .set("User-Agent", COVER_USER_AGENT)
            .set("Authorization", &format!("Bearer {}", bearer_token.trim()))
            .call()
            .map_err(|err| CoverError::Http { url: url.to_owned(), reason: err.to_string() })?;

        response
            .into_string()
            .map_err(|err| CoverError::Http { url: url.to_owned(), reason: err.to_string() })
    }

    fn get_bytes(&self, url: &str) -> Result<Vec<u8>, CoverError> {
        let parsed = ensure_https(url)?;
        let mut request = self
            .agent
            .get(url)
            .set("User-Agent", COVER_USER_AGENT)
            .set("Accept", "image/webp,image/png,image/jpeg,*/*");
        if needs_fandom_referer(&parsed) {
            request = request.set("Referer", "https://www.fandom.com/");
        }
        let response = request
            .call()
            .map_err(|err| CoverError::Http { url: url.to_owned(), reason: err.to_string() })?;

        let mut reader = response.into_reader();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut reader, &mut bytes)
            .map_err(|err| CoverError::Http { url: url.to_owned(), reason: err.to_string() })?;
        Ok(bytes)
    }
}

fn needs_fandom_referer(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        let lower = host.to_ascii_lowercase();
        lower == "fandom.com"
            || lower.ends_with(".fandom.com")
            || lower == "wikia.nocookie.net"
            || lower.ends_with(".wikia.nocookie.net")
    })
}

#[derive(Debug, Deserialize)]
struct SgDbGridResponse {
    success: bool,
    #[serde(default)]
    data: Vec<SgDbGridItem>,
}

#[derive(Debug, Deserialize)]
struct SgDbGridItem {
    url: String,
}

#[derive(Debug, Deserialize)]
struct SgDbSearchResponse {
    success: bool,
    #[serde(default)]
    data: Vec<SgDbSearchItem>,
}

#[derive(Debug, Deserialize)]
struct SgDbSearchItem {
    id: u64,
}

/// Loads local/cached cover art or fetches a 2:3 grid cover from SteamGridDB v2 when configured.
///
/// Always degrades cleanly to [`CoverLoadOutcome::Fallback`] if offline or no API key is set.
pub fn load_or_fetch_cover(client: &dyn CoverHttpClient, req: &CoverRequest) -> CoverLoadOutcome {
    if let Some(custom_path) = &req.custom_cover_art_path {
        if let Some(cover) = try_read_and_decode_file(custom_path) {
            return CoverLoadOutcome::Loaded {
                game_id: req.game_id.clone(),
                cover,
                from_disk_cache: true,
            };
        }
    }

    if let Some(cached) = find_cached_cover(&req.covers_dir, &req.game_id) {
        if let Some(cover) = try_read_and_decode_file(&cached) {
            return CoverLoadOutcome::Loaded {
                game_id: req.game_id.clone(),
                cover,
                from_disk_cache: true,
            };
        }
    }

    let mut fallback_reason = None;
    let api_key = req.api_key.trim();
    if !api_key.is_empty() {
        match fetch_from_steamgriddb(client, req, api_key) {
            Ok((raw_bytes, cover)) => {
                cache_cover_bytes(&req.covers_dir, &req.game_id, &raw_bytes);
                return CoverLoadOutcome::Loaded {
                    game_id: req.game_id.clone(),
                    cover,
                    from_disk_cache: false,
                };
            }
            Err(reason) => fallback_reason = Some(reason),
        }
    }

    if let Some(direct_url) = req.cover_art_url.as_deref().map(str::trim).filter(|u| !u.is_empty())
    {
        match fetch_direct_cover_url(client, direct_url) {
            Ok((raw_bytes, cover)) => {
                cache_cover_bytes(&req.covers_dir, &req.game_id, &raw_bytes);
                return CoverLoadOutcome::Loaded {
                    game_id: req.game_id.clone(),
                    cover,
                    from_disk_cache: false,
                };
            }
            Err(reason) => {
                fallback_reason = Some(match fallback_reason {
                    Some(prev) => format!("{prev}; {reason}"),
                    None => reason,
                });
            }
        }
    }

    CoverLoadOutcome::Fallback { game_id: req.game_id.clone(), reason: fallback_reason }
}

fn cache_cover_bytes(covers_dir: &Path, game_id: &str, raw_bytes: &[u8]) {
    let ext = detect_image_ext(raw_bytes);
    let cache_path = cache_file_path(covers_dir, game_id, ext);
    let _ = write_bytes_atomic(&cache_path, raw_bytes);
}

fn fetch_direct_cover_url(
    client: &dyn CoverHttpClient,
    url: &str,
) -> Result<(Vec<u8>, DecodedCover), String> {
    let _ = ensure_https(url).map_err(|e| e.to_string())?;
    let raw_bytes = client.get_bytes(url).map_err(|e| e.to_string())?;
    let decoded = decode_and_scale_cover(&raw_bytes)?;
    Ok((raw_bytes, decoded))
}

fn fetch_from_steamgriddb(
    client: &dyn CoverHttpClient,
    req: &CoverRequest,
    api_key: &str,
) -> Result<(Vec<u8>, DecodedCover), String> {
    let grids_url = if let Some(sgdb_id) = req.steamgriddb_id {
        format!(
            "https://www.steamgriddb.com/api/v2/grids/game/{sgdb_id}?dimensions=600x900&types=static"
        )
    } else if let Some(steam_id) = req.steam_app_id {
        format!(
            "https://www.steamgriddb.com/api/v2/grids/steam/{steam_id}?dimensions=600x900&types=static"
        )
    } else {
        let encoded = encode_path_segment(&req.title);
        let search_url =
            format!("https://www.steamgriddb.com/api/v2/search/autocomplete/{encoded}");
        let search_json = client.get_json(&search_url, api_key).map_err(|e| e.to_string())?;
        let search: SgDbSearchResponse =
            serde_json::from_str(&search_json).map_err(|e| format!("invalid search JSON: {e}"))?;
        if !search.success {
            return Err("SteamGridDB search returned success=false".to_owned());
        }
        let first_id = search
            .data
            .first()
            .map(|item| item.id)
            .ok_or_else(|| "no SteamGridDB match found".to_owned())?;
        format!(
            "https://www.steamgriddb.com/api/v2/grids/game/{first_id}?dimensions=600x900&types=static"
        )
    };

    let grids_json = client.get_json(&grids_url, api_key).map_err(|e| e.to_string())?;
    let grids: SgDbGridResponse =
        serde_json::from_str(&grids_json).map_err(|e| format!("invalid grids JSON: {e}"))?;
    if !grids.success {
        return Err("SteamGridDB grids query returned success=false".to_owned());
    }

    let img_url = grids
        .data
        .iter()
        .map(|item| item.url.as_str())
        .find(|url| url.starts_with("https://"))
        .ok_or_else(|| "no static HTTPS grid art available".to_owned())?;

    let raw_bytes = client.get_bytes(img_url).map_err(|e| e.to_string())?;
    let decoded = decode_and_scale_cover(&raw_bytes)?;
    Ok((raw_bytes, decoded))
}

fn try_read_and_decode_file(path: &Path) -> Option<DecodedCover> {
    let bytes = fs::read(path).ok()?;
    decode_and_scale_cover(&bytes).ok()
}

fn decode_and_scale_cover(bytes: &[u8]) -> Result<DecodedCover, String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("decode cover failed: {e}"))?;
    let (w, h) = img.dimensions();
    let sized = if h > MAX_COVER_HEIGHT || w > MAX_COVER_WIDTH {
        img.resize(MAX_COVER_WIDTH, MAX_COVER_HEIGHT, FilterType::Triangle)
    } else {
        img
    };
    let rgba = sized.to_rgba8();
    let (final_w, final_h) = rgba.dimensions();
    Ok(DecodedCover { width: final_w as usize, height: final_h as usize, rgba: rgba.into_raw() })
}

fn detect_image_ext(bytes: &[u8]) -> &'static str {
    match image::guess_format(bytes) {
        Ok(ImageFormat::Jpeg) => "jpg",
        Ok(ImageFormat::WebP) => "webp",
        _ => "png",
    }
}

fn find_cached_cover(covers_dir: &Path, game_id: &str) -> Option<PathBuf> {
    COVER_EXTENSIONS
        .iter()
        .map(|ext| cache_file_path(covers_dir, game_id, ext))
        .find(|path| path.is_file())
}

fn cache_file_path(covers_dir: &Path, game_id: &str, ext: &str) -> PathBuf {
    let safe_id: String = game_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    covers_dir.join(format!("{safe_id}.{ext}"))
}

fn ensure_https(url: &str) -> Result<Url, CoverError> {
    let parsed = Url::parse(url)
        .map_err(|err| CoverError::Http { url: url.to_owned(), reason: err.to_string() })?;
    if parsed.scheme() != "https" {
        return Err(CoverError::Http {
            url: url.to_owned(),
            reason: "only https:// URLs are allowed".to_owned(),
        });
    }
    Ok(parsed)
}

fn encode_path_segment(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        let is_unreserved =
            byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.' | b'~');
        if is_unreserved {
            out.push(char::from(*byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Cursor;

    use image::{ImageBuffer, Rgba};

    use super::*;

    struct MockCoverHttp {
        json_map: BTreeMap<String, String>,
        bytes_map: BTreeMap<String, Vec<u8>>,
    }

    impl CoverHttpClient for MockCoverHttp {
        fn get_json(&self, url: &str, bearer_token: &str) -> Result<String, CoverError> {
            assert_ne!(bearer_token, "");
            self.json_map.get(url).cloned().ok_or_else(|| CoverError::Http {
                url: url.to_owned(),
                reason: "mock json not found".to_owned(),
            })
        }

        fn get_bytes(&self, url: &str) -> Result<Vec<u8>, CoverError> {
            self.bytes_map.get(url).cloned().ok_or_else(|| CoverError::Http {
                url: url.to_owned(),
                reason: "mock bytes not found".to_owned(),
            })
        }
    }

    fn tiny_png_bytes() -> Vec<u8> {
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(4, 6, Rgba([240, 160, 40, 255]));
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut out, ImageFormat::Png)
            .expect("encode png");
        out.into_inner()
    }

    #[test]
    fn loads_steamgriddb_cover_caches_it_to_disk_and_handles_offline_fallback() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let covers_dir = temp.path().join("covers");
        let png = tiny_png_bytes();

        // 1. Without API key & no cache -> clean offline fallback
        let empty_client = MockCoverHttp { json_map: BTreeMap::new(), bytes_map: BTreeMap::new() };
        let no_key_req = CoverRequest {
            game_id: "nfs-most-wanted-2005".to_owned(),
            title: "Need for Speed: Most Wanted".to_owned(),
            steamgriddb_id: Some(777),
            steam_app_id: None,
            cover_art_url: None,
            custom_cover_art_path: None,
            covers_dir: covers_dir.clone(),
            api_key: String::new(),
        };
        assert!(matches!(
            load_or_fetch_cover(&empty_client, &no_key_req),
            CoverLoadOutcome::Fallback { reason: None, .. }
        ));

        // 2. With API key + autocomplete search fallback -> downloads PNG & writes disk cache
        let client = MockCoverHttp {
            json_map: BTreeMap::from([
                (
                    "https://www.steamgriddb.com/api/v2/search/autocomplete/Splinter%20Cell"
                        .to_owned(),
                    r#"{"success":true,"data":[{"id":4242}]}"#.to_owned(),
                ),
                (
                    "https://www.steamgriddb.com/api/v2/grids/game/4242?dimensions=600x900&types=static"
                        .to_owned(),
                    r#"{"success":true,"data":[{"url":"https://cdn2.steamgriddb.com/grid/test.png"}]}"#
                        .to_owned(),
                ),
            ]),
            bytes_map: BTreeMap::from([(
                "https://cdn2.steamgriddb.com/grid/test.png".to_owned(),
                png,
            )]),
        };

        let fetch_req = CoverRequest {
            game_id: "splinter-cell-pt".to_owned(),
            title: "Splinter Cell".to_owned(),
            steamgriddb_id: None,
            steam_app_id: None,
            cover_art_url: None,
            custom_cover_art_path: None,
            covers_dir: covers_dir.clone(),
            api_key: "secret-token".to_owned(),
        };
        let fetched = load_or_fetch_cover(&client, &fetch_req);
        assert!(matches!(
            fetched,
            CoverLoadOutcome::Loaded {
                from_disk_cache: false,
                cover: DecodedCover { width: 4, height: 6, .. },
                ..
            }
        ));
        assert!(covers_dir.join("splinter-cell-pt.png").is_file());

        // 3. Subsequent load hits disk cache with empty HTTP client (offline)
        let cached = load_or_fetch_cover(&empty_client, &fetch_req);
        assert!(matches!(cached, CoverLoadOutcome::Loaded { from_disk_cache: true, .. }));
    }

    #[test]
    fn fetches_and_caches_direct_recipe_cover_art_url_without_api_key() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let covers_dir = temp.path().join("covers");
        let png = tiny_png_bytes();
        let url = "https://static.wikia.nocookie.net/barbie/images/cover.png";

        let client = MockCoverHttp {
            json_map: BTreeMap::new(),
            bytes_map: BTreeMap::from([(url.to_owned(), png)]),
        };
        let req = CoverRequest {
            game_id: "barbie-diaries-2006".to_owned(),
            title: "The Barbie Diaries: High School Mystery".to_owned(),
            steamgriddb_id: None,
            steam_app_id: None,
            cover_art_url: Some(url.to_owned()),
            custom_cover_art_path: None,
            covers_dir: covers_dir.clone(),
            api_key: String::new(),
        };

        let loaded = load_or_fetch_cover(&client, &req);
        assert!(matches!(
            loaded,
            CoverLoadOutcome::Loaded {
                from_disk_cache: false,
                cover: DecodedCover { width: 4, height: 6, .. },
                ..
            }
        ));
        assert!(covers_dir.join("barbie-diaries-2006.png").is_file());

        let offline = MockCoverHttp { json_map: BTreeMap::new(), bytes_map: BTreeMap::new() };
        let cached = load_or_fetch_cover(&offline, &req);
        assert!(matches!(cached, CoverLoadOutcome::Loaded { from_disk_cache: true, .. }));
    }
}

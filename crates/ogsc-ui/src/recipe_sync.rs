//! GitHub remote recipe sync with strict validation and graceful offline fallback.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ogsc_recipes::{GameRecipe, RecipeDatabase};
use serde::Deserialize;
use thiserror::Error;
use url::Url;

use crate::settings::write_bytes_atomic;

/// Parsed GitHub repository target constructed from user settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubRepoSpec {
    /// Repository owner/organization slug.
    pub owner: String,
    /// Repository name.
    pub repo: String,
    /// Git branch or ref name.
    pub branch: String,
    /// Relative folder inside the repo containing `.json` recipe manifests.
    pub recipes_path: String,
}

impl GitHubRepoSpec {
    /// Builds the HTTPS GitHub Contents API endpoint listing `recipes_path` at `branch`.
    pub fn contents_api_url(&self) -> Result<String, SyncError> {
        let mut url = Url::parse(&format!(
            "https://api.github.com/repos/{}/{}/contents/{}",
            self.owner, self.repo, self.recipes_path
        ))
        .map_err(|err| SyncError::InvalidRepoUrl { reason: err.to_string() })?;
        url.query_pairs_mut().append_pair("ref", &self.branch);
        Ok(url.into())
    }
}

/// Errors raised while parsing GitHub sync URLs or fetching remote recipe data.
#[derive(Debug, Error)]
pub enum SyncError {
    /// Configured repository URL or path is malformed or unsafe.
    #[error("invalid GitHub recipe repository URL: {reason}")]
    InvalidRepoUrl {
        /// Explanation of why URL validation failed.
        reason: String,
    },

    /// Network, TLS, or HTTP response failure.
    #[error("HTTP request to `{url}` failed: {reason}")]
    Http {
        /// Request URL.
        url: String,
        /// Error reason.
        reason: String,
    },

    /// Remote API payload could not be parsed.
    #[error("failed to parse GitHub API JSON: {source}")]
    Parse {
        /// Serde error details.
        #[source]
        source: serde_json::Error,
    },

    /// Local file write failure.
    #[error("failed writing synced recipe `{path}`: {source}")]
    Io {
        /// Destination recipe path.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },
}

/// Outcome of a remote recipe synchronization run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Remote repository responded and valid `.json` recipes were synchronized locally.
    Synced {
        /// Count of valid remote `.json` recipes saved to the local directory.
        fetched_count: usize,
        /// Remote `.json` filenames rejected because they failed schema or security validation.
        skipped_invalid: Vec<String>,
        /// Total local recipes currently valid on disk after syncing.
        total_local_recipes: usize,
    },
    /// Remote repository could not be reached; existing bundled/local recipes remain active.
    OfflineFallback {
        /// Human-readable reason remote sync fell back to local recipes.
        reason: String,
        /// Total local recipes currently available offline.
        total_local_recipes: usize,
    },
}

/// Minimal HTTP GET abstraction enabling unit testing without live GitHub network calls.
pub trait SyncHttpClient {
    /// Performs an HTTPS GET request and returns the response body as UTF-8 text.
    fn get_text(&self, url: &str) -> Result<String, SyncError>;
}

/// Production HTTP client powered by `ureq` with timeouts and GitHub API headers.
#[derive(Debug, Clone)]
pub struct UreqSyncClient {
    agent: ureq::Agent,
}

impl Default for UreqSyncClient {
    fn default() -> Self {
        Self::new()
    }
}

impl UreqSyncClient {
    /// Constructs a new [`UreqSyncClient`] with conservative timeouts for snappy fallback.
    pub fn new() -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(20))
            .build();
        Self { agent }
    }
}

impl SyncHttpClient for UreqSyncClient {
    fn get_text(&self, url: &str) -> Result<String, SyncError> {
        let parsed = Url::parse(url)
            .map_err(|err| SyncError::Http { url: url.to_owned(), reason: err.to_string() })?;
        if parsed.scheme() != "https" {
            return Err(SyncError::Http {
                url: url.to_owned(),
                reason: "only https:// URLs are permitted for remote recipe sync".to_owned(),
            });
        }

        let response = self
            .agent
            .get(url)
            .set("User-Agent", concat!("OldGamesSecondChance/", env!("CARGO_PKG_VERSION")))
            .set("Accept", "application/vnd.github+json")
            .call()
            .map_err(|err| SyncError::Http { url: url.to_owned(), reason: err.to_string() })?;

        response
            .into_string()
            .map_err(|err| SyncError::Http { url: url.to_owned(), reason: err.to_string() })
    }
}

#[derive(Debug, Deserialize)]
struct GitHubContentEntry {
    name: String,
    #[serde(rename = "type")]
    entry_type: String,
    download_url: Option<String>,
}

/// Validates `repo_url`, `branch`, and `recipes_path` and extracts the GitHub owner/repository pair.
pub fn parse_github_repo_url(
    repo_url: &str,
    branch: &str,
    recipes_path: &str,
) -> Result<GitHubRepoSpec, SyncError> {
    let parsed = Url::parse(repo_url.trim())
        .map_err(|err| SyncError::InvalidRepoUrl { reason: err.to_string() })?;

    if parsed.scheme() != "https" {
        return Err(SyncError::InvalidRepoUrl {
            reason: "repository URL must use https://".to_owned(),
        });
    }

    let Some(host) = parsed.host_str() else {
        return Err(SyncError::InvalidRepoUrl { reason: "URL has no host".to_owned() });
    };
    if !host.eq_ignore_ascii_case("github.com") {
        return Err(SyncError::InvalidRepoUrl {
            reason: format!("expected github.com host, found `{host}`"),
        });
    }

    let segments: Vec<&str> =
        parsed.path_segments().into_iter().flatten().filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        return Err(SyncError::InvalidRepoUrl {
            reason: "expected `https://github.com/<owner>/<repo>`".to_owned(),
        });
    }

    let owner = segments[0].trim();
    let repo = segments[1].trim().trim_end_matches(".git");
    if owner.is_empty() || repo.is_empty() {
        return Err(SyncError::InvalidRepoUrl {
            reason: "repository owner and name must be non-empty".to_owned(),
        });
    }

    let clean_branch = branch.trim();
    if clean_branch.is_empty() {
        return Err(SyncError::InvalidRepoUrl { reason: "branch must not be empty".to_owned() });
    }

    let clean_path = recipes_path.trim().trim_matches('/');
    if clean_path.is_empty() {
        return Err(SyncError::InvalidRepoUrl {
            reason: "recipes subdirectory must not be empty".to_owned(),
        });
    }

    Ok(GitHubRepoSpec {
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        branch: clean_branch.to_owned(),
        recipes_path: clean_path.to_owned(),
    })
}

/// Fetches `.json` game recipes from GitHub into `target_dir`, validating every manifest first
/// and falling back gracefully to existing local recipes if offline or unreachable.
pub fn sync_recipes_from_github(
    client: &dyn SyncHttpClient,
    repo_url: &str,
    branch: &str,
    recipes_path: &str,
    target_dir: &Path,
) -> SyncOutcome {
    let existing_count = count_local_recipes(target_dir);

    let spec = match parse_github_repo_url(repo_url, branch, recipes_path) {
        Ok(spec) => spec,
        Err(err) => {
            return SyncOutcome::OfflineFallback {
                reason: err.to_string(),
                total_local_recipes: existing_count,
            };
        }
    };

    let api_url = match spec.contents_api_url() {
        Ok(url) => url,
        Err(err) => {
            return SyncOutcome::OfflineFallback {
                reason: err.to_string(),
                total_local_recipes: existing_count,
            };
        }
    };

    let listing_body = match client.get_text(&api_url) {
        Ok(body) => body,
        Err(err) => {
            return SyncOutcome::OfflineFallback {
                reason: err.to_string(),
                total_local_recipes: existing_count,
            };
        }
    };

    let entries: Vec<GitHubContentEntry> = match serde_json::from_str(&listing_body) {
        Ok(list) => list,
        Err(err) => {
            return SyncOutcome::OfflineFallback {
                reason: format!("unexpected GitHub directory payload: {err}"),
                total_local_recipes: existing_count,
            };
        }
    };

    let mut fetched_count = 0_usize;
    let mut skipped_invalid = Vec::new();

    for entry in entries {
        if entry.entry_type != "file" || !entry.name.to_ascii_lowercase().ends_with(".json") {
            continue;
        }
        let Some(safe_name) = sanitize_single_json_filename(&entry.name) else {
            skipped_invalid.push(format!("{} (unsafe filename)", entry.name));
            continue;
        };
        let Some(download_url) = entry.download_url.as_deref() else {
            skipped_invalid.push(format!("{safe_name} (missing download_url)"));
            continue;
        };

        let raw = match client.get_text(download_url) {
            Ok(text) => text,
            Err(err) => {
                skipped_invalid.push(format!("{safe_name} ({err})"));
                continue;
            }
        };

        match validate_remote_recipe_json(&raw) {
            Ok(()) => {
                let dest = target_dir.join(&safe_name);
                match write_bytes_atomic(&dest, raw.as_bytes()) {
                    Ok(()) => fetched_count += 1,
                    Err(err) => skipped_invalid.push(format!("{safe_name} (write error: {err})")),
                }
            }
            Err(reason) => {
                skipped_invalid.push(format!("{safe_name} ({reason})"));
            }
        }
    }

    let total_local_recipes = count_local_recipes(target_dir);
    SyncOutcome::Synced { fetched_count, skipped_invalid, total_local_recipes }
}

fn validate_remote_recipe_json(raw: &str) -> Result<(), String> {
    let recipe: GameRecipe =
        serde_json::from_str(raw).map_err(|err| format!("JSON parse error: {err}"))?;
    let report = recipe.validate();
    if !report.is_valid() {
        return Err(format!("failed validation ({} error(s))", report.errors.len()));
    }
    Ok(())
}

fn sanitize_single_json_filename(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains("..")
        || trimmed.starts_with('.')
        || !trimmed.to_ascii_lowercase().ends_with(".json")
    {
        return None;
    }
    Some(trimmed.to_owned())
}

fn count_local_recipes(dir: &Path) -> usize {
    if !dir.is_dir() {
        return 0;
    }
    RecipeDatabase::load_from_dir(dir, false).map_or(0, |loaded| loaded.db.len())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use super::*;

    struct MockSyncClient {
        responses: BTreeMap<String, Result<String, String>>,
    }

    impl SyncHttpClient for MockSyncClient {
        fn get_text(&self, url: &str) -> Result<String, SyncError> {
            match self.responses.get(url) {
                Some(Ok(body)) => Ok(body.clone()),
                Some(Err(reason)) => {
                    Err(SyncError::Http { url: url.to_owned(), reason: reason.clone() })
                }
                None => Err(SyncError::Http {
                    url: url.to_owned(),
                    reason: "not found in mock".to_owned(),
                }),
            }
        }
    }

    #[test]
    fn parses_github_url_and_rejects_non_https_or_non_github() {
        let spec = parse_github_repo_url(
            "https://github.com/old-games-second-chance/old-games-second-chance.git/",
            "main",
            "/recipes/games/",
        )
        .expect("parse valid URL");
        assert_eq!(
            spec.contents_api_url().expect("api url"),
            "https://api.github.com/repos/old-games-second-chance/old-games-second-chance/contents/recipes/games?ref=main"
        );

        assert!(
            parse_github_repo_url("http://github.com/owner/repo", "main", "recipes/games").is_err()
        );
        assert!(parse_github_repo_url("https://example.com/owner/repo", "main", "recipes/games")
            .is_err());
    }

    #[test]
    fn syncs_valid_remote_recipes_rejects_invalid_ones_and_handles_offline_fallback() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let target_dir = temp.path().join("recipes");

        let valid_json = include_str!("../../../recipes/games/nfs_most_wanted_2005.json");
        let listing = serde_json::json!([
            {
                "name": "nfs_most_wanted_2005.json",
                "type": "file",
                "download_url": "https://raw.githubusercontent.com/org/repo/main/recipes/games/nfs_most_wanted_2005.json"
            },
            {
                "name": "corrupt_recipe.json",
                "type": "file",
                "download_url": "https://raw.githubusercontent.com/org/repo/main/recipes/games/corrupt_recipe.json"
            }
        ])
        .to_string();

        let online_client = MockSyncClient {
            responses: BTreeMap::from([
                (
                    "https://api.github.com/repos/org/repo/contents/recipes/games?ref=main"
                        .to_owned(),
                    Ok(listing),
                ),
                (
                    "https://raw.githubusercontent.com/org/repo/main/recipes/games/nfs_most_wanted_2005.json"
                        .to_owned(),
                    Ok(valid_json.to_owned()),
                ),
                (
                    "https://raw.githubusercontent.com/org/repo/main/recipes/games/corrupt_recipe.json"
                        .to_owned(),
                    Ok("{\"bad\":true}".to_owned()),
                ),
            ]),
        };

        let outcome = sync_recipes_from_github(
            &online_client,
            "https://github.com/org/repo",
            "main",
            "recipes/games",
            &target_dir,
        );

        assert!(matches!(
            outcome,
            SyncOutcome::Synced {
                fetched_count: 1,
                total_local_recipes: 1,
                ref skipped_invalid,
            } if skipped_invalid.len() == 1
                && skipped_invalid[0].starts_with("corrupt_recipe.json (JSON parse error:")
        ));
        assert!(target_dir.join("nfs_most_wanted_2005.json").is_file());
        assert!(!target_dir.join("corrupt_recipe.json").exists());

        // Offline fallback preserves the existing local recipe without erroring out
        let offline_client = MockSyncClient {
            responses: BTreeMap::from([(
                "https://api.github.com/repos/org/repo/contents/recipes/games?ref=main".to_owned(),
                Err("network unreachable".to_owned()),
            )]),
        };
        let offline = sync_recipes_from_github(
            &offline_client,
            "https://github.com/org/repo",
            "main",
            "recipes/games",
            &target_dir,
        );
        assert!(matches!(offline, SyncOutcome::OfflineFallback { total_local_recipes: 1, .. }));
        assert!(fs::read_to_string(target_dir.join("nfs_most_wanted_2005.json")).is_ok());
    }
}

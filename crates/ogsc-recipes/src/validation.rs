//! Semantic, structural, and security validation for [`GameRecipe`] manifests.

use std::collections::{BTreeSet, HashSet};

use url::Url;

use crate::schema::{
    GameRecipe, HashVerificationStatus, KnownIssue, LaunchConfig, RecipeMetadata,
    RuntimeRequirements, SandboxConfig, CURRENT_SCHEMA_VERSION,
};
use crate::steps::{
    AppCompatFlag, BinaryPatchSource, CpuAffinityMode, DllSource, FixOption, IniValue, PatchAction,
    PatchStep, RegistryHive, RollbackAction, RollbackPlan,
};
use crate::wrapper::{DllChainRule, WrapperDeployment, WrapperKind};

/// Aggregated errors and non-fatal warnings emitted while validating a [`GameRecipe`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidationReport {
    /// Fatal structural, cross-reference, or security errors that block loading/execution.
    pub errors: Vec<ValidationIssue>,
    /// Non-fatal warnings (for example `unverified_template` placeholder hashes or HKLM writes).
    pub warnings: Vec<ValidationIssue>,
}

impl ValidationReport {
    /// Returns `true` when zero errors were found (warnings may still be present).
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }

    /// Returns `true` when one or more warnings were emitted.
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }

    /// Converts this report into a `Result`, failing on errors or (when `strict == true`) on warnings.
    pub fn into_result(self, strict: bool) -> Result<(), Self> {
        if !self.is_valid() || (strict && self.has_warnings()) {
            Err(self)
        } else {
            Ok(())
        }
    }

    fn push_error(
        &mut self,
        field: impl Into<String>,
        code: ValidationCode,
        message: impl Into<String>,
    ) {
        self.errors.push(ValidationIssue { field: field.into(), code, message: message.into() });
    }

    fn push_warning(
        &mut self,
        field: impl Into<String>,
        code: ValidationCode,
        message: impl Into<String>,
    ) {
        self.warnings.push(ValidationIssue { field: field.into(), code, message: message.into() });
    }
}

/// A single error or warning identified within a recipe field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationIssue {
    /// JSON-like path to the offending field (for example `steps[1].action.url`).
    pub field: String,
    /// Machine-readable classification code.
    pub code: ValidationCode,
    /// Human-readable diagnostic message.
    pub message: String,
}

/// Stable diagnostic code categorizing validation findings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValidationCode {
    /// Unsupported `schema_version` integer.
    UnsupportedSchemaVersion,
    /// Invalid lowercase identifier slug.
    InvalidSlug,
    /// Duplicate identifier within a list (`releases`, `known_issues`, `steps`).
    DuplicateId,
    /// Required text field or collection is empty.
    EmptyRequiredField,
    /// Numeric field outside accepted bounds.
    OutOfRange,
    /// Malformed SemVer string.
    InvalidSemver,
    /// Malformed ISO-8601 `YYYY-MM-DD` date.
    InvalidDate,
    /// URL is malformed or uses a scheme other than `https`.
    InsecureOrInvalidUrl,
    /// Path is absolute, escapes via `..`, uses UNC/drive syntax, or hits a Windows device name.
    UnsafeRelativePath,
    /// Digest string has the wrong length or contains non-lowercase-hex characters.
    InvalidHexDigest,
    /// Release fingerprint lacks any cryptographic hash or partial hash.
    MissingFingerprintHash,
    /// Referenced step ID or release ID does not exist (or download step appears out of order).
    BrokenCrossReference,
    /// Action-specific invariant violation (for example identical original/patched SHA-256).
    InvalidActionConfiguration,
    /// Floating-point INI value is NaN or infinite.
    NonFiniteFloat,
    /// Recipe uses `unverified_template` hashes (blocked under `--strict`).
    UnverifiedTemplateHash,
    /// Recipe writes compatibility flags to `HKLM` instead of per-user `HKCU`.
    HklmRegistryWrite,
    /// Blocker issue has neither a non-optional automated fix nor user BYOG guidance.
    UnmitigatedBlockerIssue,
}

impl GameRecipe {
    /// Validates structural, security, and cross-reference invariants across the entire recipe.
    pub fn validate(&self) -> ValidationReport {
        let mut report = ValidationReport::default();

        validate_header(self, &mut report);

        let release_ids = validate_releases(&self.releases, &mut report);
        validate_metadata(&self.metadata, &release_ids, &mut report);
        validate_runtime_requirements(&self.runtime_requirements, &self.launch, &mut report);

        if let Some(sandbox) = &self.sandbox {
            validate_sandbox(sandbox, &mut report);
        }

        let fix_info = validate_fixes(&self.fixes, &release_ids, &mut report);
        validate_launch(&self.launch, &release_ids, &fix_info.all_fix_ids, &mut report);
        validate_known_issues(&self.known_issues, &fix_info, &mut report);
        validate_rollback(&self.rollback, &mut report);

        report
    }
}

struct ValidatedFixes {
    all_fix_ids: HashSet<String>,
    recommended_fix_ids: HashSet<String>,
}

fn validate_header(recipe: &GameRecipe, report: &mut ValidationReport) {
    if recipe.schema_version != CURRENT_SCHEMA_VERSION {
        report.push_error(
            "schema_version",
            ValidationCode::UnsupportedSchemaVersion,
            format!(
                "expected schema_version {CURRENT_SCHEMA_VERSION}, found {}",
                recipe.schema_version
            ),
        );
    }

    if !is_valid_slug(&recipe.game_id) {
        report.push_error(
            "game_id",
            ValidationCode::InvalidSlug,
            format!(
                "invalid game_id `{}`: must be 1–64 chars matching `[a-z0-9][a-z0-9._-]{{0,63}}`",
                recipe.game_id
            ),
        );
    }

    if recipe.title.trim().is_empty() {
        report.push_error(
            "title",
            ValidationCode::EmptyRequiredField,
            "game title must not be empty",
        );
    }

    if !(1980..=2030).contains(&recipe.release_year) {
        report.push_error(
            "release_year",
            ValidationCode::OutOfRange,
            format!("release_year {} is outside supported range 1980..=2030", recipe.release_year),
        );
    }
}

fn validate_metadata(
    metadata: &RecipeMetadata,
    release_ids: &BTreeSet<String>,
    report: &mut ValidationReport,
) {
    if !is_valid_semver(&metadata.recipe_version) {
        report.push_error(
            "metadata.recipe_version",
            ValidationCode::InvalidSemver,
            format!(
                "recipe_version `{}` must follow SemVer `MAJOR.MINOR.PATCH`",
                metadata.recipe_version
            ),
        );
    }

    if metadata.authors.is_empty() || metadata.authors.iter().any(|a| a.trim().is_empty()) {
        report.push_error(
            "metadata.authors",
            ValidationCode::EmptyRequiredField,
            "metadata.authors must contain at least one non-empty author string",
        );
    }

    if metadata.license.trim().is_empty() {
        report.push_error(
            "metadata.license",
            ValidationCode::EmptyRequiredField,
            "metadata.license must not be empty",
        );
    }

    if !is_valid_iso_date(&metadata.updated_at) {
        report.push_error(
            "metadata.updated_at",
            ValidationCode::InvalidDate,
            format!(
                "updated_at `{}` must be a valid YYYY-MM-DD calendar date",
                metadata.updated_at
            ),
        );
    }

    if let Some(url) = &metadata.pcgamingwiki_url {
        validate_https_url(url, "metadata.pcgamingwiki_url", report);
    }

    if let Some(url) = &metadata.cover_art_url {
        validate_https_url(url, "metadata.cover_art_url", report);
    }

    if let Some(req_ver) = &metadata.required_game_version {
        if req_ver.trim().is_empty() {
            report.push_error(
                "metadata.required_game_version",
                ValidationCode::EmptyRequiredField,
                "required_game_version must not be empty when specified",
            );
        }
    }

    for (idx, rel_id) in metadata.incompatible_releases.iter().enumerate() {
        if !release_ids.contains(rel_id) {
            report.push_error(
                format!("metadata.incompatible_releases[{idx}]"),
                ValidationCode::BrokenCrossReference,
                format!("incompatible_releases references unknown release_id `{rel_id}`"),
            );
        }
    }
}

fn validate_releases(
    releases: &[crate::schema::GameRelease],
    report: &mut ValidationReport,
) -> BTreeSet<String> {
    let mut release_ids = BTreeSet::new();

    if releases.is_empty() {
        report.push_error(
            "releases",
            ValidationCode::EmptyRequiredField,
            "recipe must declare at least one recognized game release",
        );
        return release_ids;
    }

    for (idx, rel) in releases.iter().enumerate() {
        let prefix = format!("releases[{idx}]");
        if !is_valid_slug(&rel.release_id) {
            report.push_error(
                format!("{prefix}.release_id"),
                ValidationCode::InvalidSlug,
                format!("invalid release_id `{}`", rel.release_id),
            );
        } else if !release_ids.insert(rel.release_id.clone()) {
            report.push_error(
                format!("{prefix}.release_id"),
                ValidationCode::DuplicateId,
                format!("duplicate release_id `{}`", rel.release_id),
            );
        }

        if rel.label.trim().is_empty() {
            report.push_error(
                format!("{prefix}.label"),
                ValidationCode::EmptyRequiredField,
                "release label must not be empty",
            );
        }

        if rel.version.trim().is_empty() {
            report.push_error(
                format!("{prefix}.version"),
                ValidationCode::EmptyRequiredField,
                "release version must not be empty",
            );
        }

        validate_relative_path(
            &rel.executable_path,
            false,
            &format!("{prefix}.executable_path"),
            report,
        );

        for (s_idx, sentinel) in rel.sentinel_files.iter().enumerate() {
            validate_relative_path(
                sentinel,
                false,
                &format!("{prefix}.sentinel_files[{s_idx}]"),
                report,
            );
        }

        validate_fingerprint(&rel.fingerprint, &format!("{prefix}.fingerprint"), report);
    }

    release_ids
}

fn validate_fingerprint(
    fp: &crate::schema::ExecutableFingerprint,
    prefix: &str,
    report: &mut ValidationReport,
) {
    if fp.file_size_bytes == Some(0) {
        report.push_error(
            format!("{prefix}.file_size_bytes"),
            ValidationCode::OutOfRange,
            "file_size_bytes cannot be 0",
        );
    }

    let has_any_hash = fp.sha256.is_some() || fp.md5.is_some() || fp.partial_sha256.is_some();
    if !has_any_hash {
        report.push_error(
            prefix,
            ValidationCode::MissingFingerprintHash,
            "fingerprint must specify at least one of sha256, md5, or partial_sha256",
        );
    }

    if let Some(sha) = &fp.sha256 {
        validate_hex_digest(sha, 64, "SHA-256", &format!("{prefix}.sha256"), report);
    }

    if let Some(md5) = &fp.md5 {
        validate_hex_digest(md5, 32, "MD5", &format!("{prefix}.md5"), report);
    }

    if let Some(partial) = &fp.partial_sha256 {
        if partial.length == 0 {
            report.push_error(
                format!("{prefix}.partial_sha256.length"),
                ValidationCode::OutOfRange,
                "partial_sha256.length must be greater than 0",
            );
        }
        validate_hex_digest(
            &partial.sha256,
            64,
            "SHA-256",
            &format!("{prefix}.partial_sha256.sha256"),
            report,
        );
    }

    if fp.verification_status == HashVerificationStatus::UnverifiedTemplate {
        report.push_warning(
            format!("{prefix}.verification_status"),
            ValidationCode::UnverifiedTemplateHash,
            "fingerprint still uses an unverified template hash; verify against a real binary before release",
        );
    }
}

fn validate_runtime_requirements(
    req: &RuntimeRequirements,
    launch: &LaunchConfig,
    report: &mut ValidationReport,
) {
    if req.requires_16bit_emulator && launch.emulator_hook.is_none() {
        report.push_error(
            "launch.emulator_hook",
            ValidationCode::BrokenCrossReference,
            "runtime_requirements.requires_16bit_emulator is true, so launch.emulator_hook must be configured",
        );
    }
}

fn validate_sandbox(sandbox: &SandboxConfig, report: &mut ValidationReport) {
    validate_relative_path(&sandbox.sandbox_root, false, "sandbox.sandbox_root", report);
    for (idx, subpath) in sandbox.known_save_subpaths.iter().enumerate() {
        validate_relative_path(
            subpath,
            false,
            &format!("sandbox.known_save_subpaths[{idx}]"),
            report,
        );
    }
}

fn validate_launch(
    launch: &LaunchConfig,
    release_ids: &BTreeSet<String>,
    fix_ids: &HashSet<String>,
    report: &mut ValidationReport,
) {
    validate_relative_path(&launch.default_executable, false, "launch.default_executable", report);

    for (release_id, exe) in &launch.release_executable_overrides {
        let field = format!("launch.release_executable_overrides.{release_id}");
        if !release_ids.contains(release_id) {
            report.push_error(
                &field,
                ValidationCode::BrokenCrossReference,
                format!("unknown release_id `{release_id}` in launch.release_executable_overrides"),
            );
        }
        validate_relative_path(exe, false, &field, report);
    }

    for (fix_id, exe) in &launch.fix_executable_overrides {
        let field = format!("launch.fix_executable_overrides.{fix_id}");
        if !fix_ids.contains(fix_id) {
            report.push_error(
                &field,
                ValidationCode::BrokenCrossReference,
                format!("unknown fix_id `{fix_id}` in launch.fix_executable_overrides"),
            );
        }
        validate_relative_path(exe, false, &field, report);
    }

    if let Some(wd) = &launch.working_directory {
        validate_relative_path(wd, true, "launch.working_directory", report);
    }

    if let Some(hook) = &launch.emulator_hook {
        validate_relative_path(
            &hook.config_file,
            false,
            "launch.emulator_hook.config_file",
            report,
        );
    }
}

fn validate_fixes(
    fixes: &[FixOption],
    release_ids: &BTreeSet<String>,
    report: &mut ValidationReport,
) -> ValidatedFixes {
    let mut all_fix_ids = HashSet::new();
    let mut recommended_fix_ids = HashSet::new();
    let mut global_step_ids = HashSet::new();

    for (f_idx, fix) in fixes.iter().enumerate() {
        let fix_prefix = format!("fixes[{f_idx}]");
        if !is_valid_slug(&fix.id) {
            report.push_error(
                format!("{fix_prefix}.id"),
                ValidationCode::InvalidSlug,
                format!("invalid fix id `{}`", fix.id),
            );
        } else if !all_fix_ids.insert(fix.id.clone()) {
            report.push_error(
                format!("{fix_prefix}.id"),
                ValidationCode::DuplicateId,
                format!("duplicate fix id `{}`", fix.id),
            );
        }

        if fix.recommended {
            recommended_fix_ids.insert(fix.id.clone());
        }

        if fix.title.trim().is_empty() {
            report.push_error(
                format!("{fix_prefix}.title"),
                ValidationCode::EmptyRequiredField,
                "fix title must not be empty",
            );
        }

        if fix.description.trim().is_empty() {
            report.push_error(
                format!("{fix_prefix}.description"),
                ValidationCode::EmptyRequiredField,
                "fix description must not be empty",
            );
        }

        for (r_idx, rel_id) in fix.applies_to_releases.iter().enumerate() {
            if !release_ids.contains(rel_id) {
                report.push_error(
                    format!("{fix_prefix}.applies_to_releases[{r_idx}]"),
                    ValidationCode::BrokenCrossReference,
                    format!("fix `{}` references unknown release_id `{rel_id}`", fix.id),
                );
            }
        }

        if fix.steps.is_empty() {
            report.push_error(
                format!("{fix_prefix}.steps"),
                ValidationCode::EmptyRequiredField,
                format!("fix `{}` must define at least one step", fix.id),
            );
            continue;
        }

        // Scoped per FixOption so every selectable fix group stays completely self-contained.
        let mut fix_prior_download_step_ids = HashSet::new();
        for (s_idx, step) in fix.steps.iter().enumerate() {
            let step_prefix = format!("{fix_prefix}.steps[{s_idx}]");
            validate_single_step(
                step,
                &step_prefix,
                release_ids,
                &mut global_step_ids,
                &mut fix_prior_download_step_ids,
                report,
            );
        }
    }

    ValidatedFixes { all_fix_ids, recommended_fix_ids }
}

fn validate_single_step(
    step: &PatchStep,
    prefix: &str,
    release_ids: &BTreeSet<String>,
    global_step_ids: &mut HashSet<String>,
    prior_download_step_ids: &mut HashSet<String>,
    report: &mut ValidationReport,
) {
    if !is_valid_slug(&step.id) {
        report.push_error(
            format!("{prefix}.id"),
            ValidationCode::InvalidSlug,
            format!("invalid step id `{}`", step.id),
        );
    } else if !global_step_ids.insert(step.id.clone()) {
        report.push_error(
            format!("{prefix}.id"),
            ValidationCode::DuplicateId,
            format!("duplicate step id `{}`", step.id),
        );
    }

    if step.name.trim().is_empty() {
        report.push_error(
            format!("{prefix}.name"),
            ValidationCode::EmptyRequiredField,
            "step name must not be empty",
        );
    }

    if step.description.trim().is_empty() {
        report.push_error(
            format!("{prefix}.description"),
            ValidationCode::EmptyRequiredField,
            "step description must not be empty",
        );
    }

    for (r_idx, rel_id) in step.applies_to_releases.iter().enumerate() {
        if !release_ids.contains(rel_id) {
            report.push_error(
                format!("{prefix}.applies_to_releases[{r_idx}]"),
                ValidationCode::BrokenCrossReference,
                format!("step `{}` references unknown release_id `{rel_id}`", step.id),
            );
        }
    }

    if let Some(os_filter) = step.os_filter {
        if let (Some(min_b), Some(max_b)) =
            (os_filter.min_windows_build, os_filter.max_windows_build)
        {
            if min_b > max_b {
                report.push_error(
                    format!("{prefix}.os_filter"),
                    ValidationCode::OutOfRange,
                    format!(
                        "os_filter min_windows_build ({min_b}) cannot exceed max_windows_build ({max_b})"
                    ),
                );
            }
        }
    }

    validate_patch_action(
        &step.action,
        &format!("{prefix}.action"),
        prior_download_step_ids,
        report,
    );

    if matches!(step.action, PatchAction::DownloadVerifiedZip { .. }) {
        prior_download_step_ids.insert(step.id.clone());
    }
}

fn validate_patch_action(
    action: &PatchAction,
    prefix: &str,
    prior_download_step_ids: &HashSet<String>,
    report: &mut ValidationReport,
) {
    match action {
        PatchAction::DownloadVerifiedZip { .. } => validate_download_action(action, prefix, report),
        PatchAction::ExtractDll { .. } => {
            validate_extract_dll_action(action, prefix, prior_download_step_ids, report);
        }
        PatchAction::DeployWrapper(deployment) => {
            validate_wrapper_deployment(deployment, prefix, report);
        }
        PatchAction::ApplyBinaryBsdiffPatch { .. } => {
            validate_bsdiff_action(action, prefix, report);
        }
        PatchAction::SetRegistryCompatFlags { .. } => {
            validate_registry_flags_action(action, prefix, report);
        }
        PatchAction::WriteIniConfig { target_file, sections, .. } => {
            validate_relative_path(target_file, false, &format!("{prefix}.target_file"), report);
            validate_ini_sections(sections, &format!("{prefix}.sections"), report);
        }
        PatchAction::LimitCpuAffinity { mode, .. } => {
            validate_cpu_affinity(mode, &format!("{prefix}.mode"), report);
        }
        PatchAction::ConfigureSandbox { create_directories, .. } => {
            for (d_idx, dir) in create_directories.iter().enumerate() {
                validate_relative_path(
                    dir,
                    false,
                    &format!("{prefix}.create_directories[{d_idx}]"),
                    report,
                );
            }
        }
    }
}

fn validate_download_action(action: &PatchAction, prefix: &str, report: &mut ValidationReport) {
    let PatchAction::DownloadVerifiedZip {
        url,
        mirror_urls,
        sha256,
        verification_status,
        size_bytes,
        extract_to,
        include_patterns,
        license,
        source_repo,
        ..
    } = action
    else {
        return;
    };

    validate_https_url(url, &format!("{prefix}.url"), report);
    for (m_idx, mirror) in mirror_urls.iter().enumerate() {
        validate_https_url(mirror, &format!("{prefix}.mirror_urls[{m_idx}]"), report);
    }
    if let Some(repo) = source_repo {
        validate_https_url(repo, &format!("{prefix}.source_repo"), report);
    }
    validate_hex_digest(sha256, 64, "SHA-256", &format!("{prefix}.sha256"), report);
    if *verification_status == HashVerificationStatus::UnverifiedTemplate {
        report.push_warning(
            format!("{prefix}.verification_status"),
            ValidationCode::UnverifiedTemplateHash,
            "download archive hash is marked unverified_template",
        );
    }
    if *size_bytes == Some(0) {
        report.push_error(
            format!("{prefix}.size_bytes"),
            ValidationCode::OutOfRange,
            "download size_bytes cannot be 0",
        );
    }
    validate_relative_path(extract_to, true, &format!("{prefix}.extract_to"), report);
    for (p_idx, pattern) in include_patterns.iter().enumerate() {
        validate_glob_pattern(pattern, &format!("{prefix}.include_patterns[{p_idx}]"), report);
    }
    if license.trim().is_empty() {
        report.push_error(
            format!("{prefix}.license"),
            ValidationCode::EmptyRequiredField,
            "download step license must not be empty",
        );
    }
}

fn validate_extract_dll_action(
    action: &PatchAction,
    prefix: &str,
    prior_download_step_ids: &HashSet<String>,
    report: &mut ValidationReport,
) {
    let PatchAction::ExtractDll {
        source,
        destination_path,
        expected_sha256,
        verification_status,
        ..
    } = action
    else {
        return;
    };

    validate_dll_source(source, &format!("{prefix}.source"), prior_download_step_ids, report);
    validate_relative_path(destination_path, false, &format!("{prefix}.destination_path"), report);
    if !has_allowed_extension(destination_path, &[".dll", ".asi", ".flt"]) {
        report.push_error(
            format!("{prefix}.destination_path"),
            ValidationCode::InvalidActionConfiguration,
            format!(
                "extract_dll destination `{destination_path}` must end with .dll, .asi, or .flt"
            ),
        );
    }
    if let Some(sha) = expected_sha256 {
        validate_hex_digest(sha, 64, "SHA-256", &format!("{prefix}.expected_sha256"), report);
        if *verification_status == HashVerificationStatus::UnverifiedTemplate {
            report.push_warning(
                format!("{prefix}.verification_status"),
                ValidationCode::UnverifiedTemplateHash,
                "expected_sha256 is marked unverified_template",
            );
        }
    }
}

fn validate_bsdiff_action(action: &PatchAction, prefix: &str, report: &mut ValidationReport) {
    let PatchAction::ApplyBinaryBsdiffPatch {
        target_file,
        patch_source,
        expected_original_sha256,
        expected_patched_sha256,
        verification_status,
        ..
    } = action
    else {
        return;
    };

    validate_relative_path(target_file, false, &format!("{prefix}.target_file"), report);
    validate_binary_patch_source(patch_source, &format!("{prefix}.patch_source"), report);
    validate_hex_digest(
        expected_original_sha256,
        64,
        "SHA-256",
        &format!("{prefix}.expected_original_sha256"),
        report,
    );
    validate_hex_digest(
        expected_patched_sha256,
        64,
        "SHA-256",
        &format!("{prefix}.expected_patched_sha256"),
        report,
    );
    if expected_original_sha256 == expected_patched_sha256 {
        report.push_error(
            format!("{prefix}.expected_patched_sha256"),
            ValidationCode::InvalidActionConfiguration,
            "expected_original_sha256 and expected_patched_sha256 must differ",
        );
    }
    if *verification_status == HashVerificationStatus::UnverifiedTemplate {
        report.push_warning(
            format!("{prefix}.verification_status"),
            ValidationCode::UnverifiedTemplateHash,
            "binary bsdiff patch hashes are marked unverified_template",
        );
    }
}

fn validate_registry_flags_action(
    action: &PatchAction,
    prefix: &str,
    report: &mut ValidationReport,
) {
    let PatchAction::SetRegistryCompatFlags { hive, target_executable, flags, .. } = action else {
        return;
    };

    validate_relative_path(
        target_executable,
        false,
        &format!("{prefix}.target_executable"),
        report,
    );
    if flags.is_empty() {
        report.push_error(
            format!("{prefix}.flags"),
            ValidationCode::EmptyRequiredField,
            "set_registry_compat_flags must specify at least one flag",
        );
    }
    for (f_idx, flag) in flags.iter().enumerate() {
        if let AppCompatFlag::Custom(token) = flag {
            if token.is_empty()
                || !token.is_ascii()
                || token.chars().any(|c| c.is_ascii_whitespace() || c.is_control())
            {
                report.push_error(
                    format!("{prefix}.flags[{f_idx}]"),
                    ValidationCode::InvalidActionConfiguration,
                    format!(
                        "custom AppCompat token `{token}` must be non-empty ASCII without whitespace"
                    ),
                );
            }
        }
    }
    if *hive == RegistryHive::Hklm {
        report.push_warning(
            format!("{prefix}.hive"),
            ValidationCode::HklmRegistryWrite,
            "HKLM AppCompatFlags requires elevation and modifies machine-wide state; prefer `hkcu`",
        );
    }
}

fn validate_dll_source(
    source: &DllSource,
    prefix: &str,
    prior_download_step_ids: &HashSet<String>,
    report: &mut ValidationReport,
) {
    match source {
        DllSource::BundledToolkit { toolkit, relative_path } => {
            if !is_valid_slug(toolkit) {
                report.push_error(
                    format!("{prefix}.toolkit"),
                    ValidationCode::InvalidSlug,
                    format!("invalid toolkit identifier `{toolkit}`"),
                );
            }
            validate_relative_path(
                relative_path,
                false,
                &format!("{prefix}.relative_path"),
                report,
            );
        }
        DllSource::CachedArchive { download_step_id, archive_member } => {
            if !prior_download_step_ids.contains(download_step_id) {
                report.push_error(
                    format!("{prefix}.download_step_id"),
                    ValidationCode::BrokenCrossReference,
                    format!(
                        "cached_archive references `{download_step_id}`, which must be an earlier `download_verified_zip` step"
                    ),
                );
            }
            validate_relative_path(
                archive_member,
                false,
                &format!("{prefix}.archive_member"),
                report,
            );
        }
    }
}

fn validate_wrapper_deployment(
    deployment: &WrapperDeployment,
    prefix: &str,
    report: &mut ValidationReport,
) {
    validate_relative_path(&deployment.target_dir, true, &format!("{prefix}.target_dir"), report);

    for (idx, rule) in deployment.dll_chain_renames.iter().enumerate() {
        validate_dll_chain_rule(rule, &format!("{prefix}.dll_chain_renames[{idx}]"), report);
    }

    match &deployment.wrapper {
        WrapperKind::DgVoodoo2 { api_targets, .. } => {
            if api_targets.is_empty() {
                report.push_error(
                    format!("{prefix}.wrapper.api_targets"),
                    ValidationCode::EmptyRequiredField,
                    "dgVoodoo2 deployment requires at least one api_target",
                );
            }
        }
        WrapperKind::DxWrapper { stub_dlls, .. } => {
            validate_dll_filename_list(stub_dlls, &format!("{prefix}.wrapper.stub_dlls"), report);
        }
        WrapperKind::WineD3d { dlls, .. } => {
            validate_dll_filename_list(dlls, &format!("{prefix}.wrapper.dlls"), report);
        }
        WrapperKind::XInputPlus { target_dlls, .. } => {
            validate_dll_filename_list(
                target_dlls,
                &format!("{prefix}.wrapper.target_dlls"),
                report,
            );
        }
        WrapperKind::DosBoxX { machine, .. } => {
            if machine.trim().is_empty() {
                report.push_error(
                    format!("{prefix}.wrapper.machine"),
                    ValidationCode::EmptyRequiredField,
                    "dosbox_x machine must not be empty",
                );
            }
        }
        WrapperKind::Box86 { machine_profile, .. } => {
            if machine_profile.trim().is_empty() {
                report.push_error(
                    format!("{prefix}.wrapper.machine_profile"),
                    ValidationCode::EmptyRequiredField,
                    "86box machine_profile must not be empty",
                );
            }
        }
    }
}

fn validate_dll_chain_rule(rule: &DllChainRule, prefix: &str, report: &mut ValidationReport) {
    validate_single_dll_filename(&rule.original_dll, &format!("{prefix}.original_dll"), report);
    validate_single_dll_filename(&rule.chained_as, &format!("{prefix}.chained_as"), report);
    if rule.original_dll.eq_ignore_ascii_case(&rule.chained_as) {
        report.push_error(
            format!("{prefix}.chained_as"),
            ValidationCode::InvalidActionConfiguration,
            "dll_chain_renames original_dll and chained_as must differ",
        );
    }
}

fn validate_dll_filename_list(dlls: &[String], field: &str, report: &mut ValidationReport) {
    if dlls.is_empty() {
        report.push_error(
            field,
            ValidationCode::EmptyRequiredField,
            "wrapper DLL list must not be empty",
        );
        return;
    }
    for (idx, dll) in dlls.iter().enumerate() {
        validate_single_dll_filename(dll, &format!("{field}[{idx}]"), report);
    }
}

fn validate_single_dll_filename(name: &str, field: &str, report: &mut ValidationReport) {
    validate_relative_path(name, false, field, report);
    if name.contains('/') || name.contains('\\') || !has_allowed_extension(name, &[".dll"]) {
        report.push_error(
            field,
            ValidationCode::InvalidActionConfiguration,
            format!("expected a bare `.dll` filename without path separators, found `{name}`"),
        );
    }
}

fn validate_binary_patch_source(
    source: &BinaryPatchSource,
    prefix: &str,
    report: &mut ValidationReport,
) {
    match source {
        BinaryPatchSource::Download { url, sha256 } => {
            validate_https_url(url, &format!("{prefix}.url"), report);
            validate_hex_digest(sha256, 64, "SHA-256", &format!("{prefix}.sha256"), report);
        }
        BinaryPatchSource::LocalManifestRelative { path, sha256 } => {
            validate_relative_path(path, false, &format!("{prefix}.path"), report);
            validate_hex_digest(sha256, 64, "SHA-256", &format!("{prefix}.sha256"), report);
        }
    }
}

fn validate_ini_sections(
    sections: &std::collections::BTreeMap<String, std::collections::BTreeMap<String, IniValue>>,
    prefix: &str,
    report: &mut ValidationReport,
) {
    if sections.is_empty() {
        report.push_error(
            prefix,
            ValidationCode::EmptyRequiredField,
            "write_ini_config must contain at least one section",
        );
        return;
    }

    for (section, keys) in sections {
        let sec_field = format!("{prefix}.{section}");
        if section.trim().is_empty()
            || section.contains('\r')
            || section.contains('\n')
            || section.contains('[')
            || section.contains(']')
        {
            report.push_error(
                &sec_field,
                ValidationCode::InvalidActionConfiguration,
                format!("invalid INI section name `{section}`"),
            );
        }
        if keys.is_empty() {
            report.push_error(
                &sec_field,
                ValidationCode::EmptyRequiredField,
                format!("INI section `[{section}]` must define at least one key"),
            );
            continue;
        }
        for (key, value) in keys {
            let key_field = format!("{sec_field}.{key}");
            if key.trim().is_empty()
                || key.contains('\r')
                || key.contains('\n')
                || key.contains('=')
            {
                report.push_error(
                    &key_field,
                    ValidationCode::InvalidActionConfiguration,
                    format!("invalid INI key name `{key}`"),
                );
            }
            if let IniValue::Float(v) = value {
                if !v.is_finite() {
                    report.push_error(
                        &key_field,
                        ValidationCode::NonFiniteFloat,
                        format!("INI float value for `{key}` must be finite"),
                    );
                }
            }
        }
    }
}

fn validate_cpu_affinity(mode: &CpuAffinityMode, field: &str, report: &mut ValidationReport) {
    match *mode {
        CpuAffinityMode::MaxLogicalCores { count } => {
            if count == 0 {
                report.push_error(
                    format!("{field}.count"),
                    ValidationCode::OutOfRange,
                    "max_logical_cores count must be >= 1",
                );
            }
        }
        CpuAffinityMode::PhysicalCoresOnly { max_cores: Some(0) } => {
            report.push_error(
                format!("{field}.max_cores"),
                ValidationCode::OutOfRange,
                "physical_cores_only max_cores must be >= 1 when specified",
            );
        }
        CpuAffinityMode::ExplicitMask { mask } => {
            if mask == 0 {
                report.push_error(
                    format!("{field}.mask"),
                    ValidationCode::OutOfRange,
                    "explicit_mask must be non-zero",
                );
            }
        }
        CpuAffinityMode::SingleCore | CpuAffinityMode::PhysicalCoresOnly { .. } => {}
    }
}

fn validate_known_issues(
    issues: &[KnownIssue],
    fixes: &ValidatedFixes,
    report: &mut ValidationReport,
) {
    let mut seen_issue_ids = HashSet::new();

    for (idx, issue) in issues.iter().enumerate() {
        let prefix = format!("known_issues[{idx}]");
        if !is_valid_slug(&issue.id) {
            report.push_error(
                format!("{prefix}.id"),
                ValidationCode::InvalidSlug,
                format!("invalid issue id `{}`", issue.id),
            );
        } else if !seen_issue_ids.insert(issue.id.clone()) {
            report.push_error(
                format!("{prefix}.id"),
                ValidationCode::DuplicateId,
                format!("duplicate issue id `{}`", issue.id),
            );
        }

        if issue.summary.trim().is_empty() {
            report.push_error(
                format!("{prefix}.summary"),
                ValidationCode::EmptyRequiredField,
                "issue summary must not be empty",
            );
        }
        if issue.details.trim().is_empty() {
            report.push_error(
                format!("{prefix}.details"),
                ValidationCode::EmptyRequiredField,
                "issue details must not be empty",
            );
        }

        for (f_idx, fix_id) in issue.fixed_by_fixes.iter().enumerate() {
            if !fixes.all_fix_ids.contains(fix_id) {
                report.push_error(
                    format!("{prefix}.fixed_by_fixes[{f_idx}]"),
                    ValidationCode::BrokenCrossReference,
                    format!("issue `{}` references unknown fix id `{fix_id}`", issue.id),
                );
            }
        }

        if issue.severity == crate::schema::IssueSeverity::Blocker {
            let has_recommended_fix =
                issue.fixed_by_fixes.iter().any(|id| fixes.recommended_fix_ids.contains(id));
            let has_user_guidance =
                issue.user_action_required.as_deref().is_some_and(|s| !s.trim().is_empty());

            if !has_recommended_fix && !has_user_guidance {
                report.push_warning(
                    prefix,
                    ValidationCode::UnmitigatedBlockerIssue,
                    format!(
                        "blocker issue `{}` has neither a recommended fix nor user_action_required guidance",
                        issue.id
                    ),
                );
            }
        }
    }
}

fn validate_rollback(rollback: &RollbackPlan, report: &mut ValidationReport) {
    validate_relative_path(&rollback.backup_directory, false, "rollback.backup_directory", report);

    for (idx, path) in rollback.extra_cleanup_paths.iter().enumerate() {
        validate_relative_path(
            path,
            false,
            &format!("rollback.extra_cleanup_paths[{idx}]"),
            report,
        );
    }

    for (idx, action) in rollback.custom_actions.iter().enumerate() {
        let prefix = format!("rollback.custom_actions[{idx}]");
        match action {
            RollbackAction::DeleteFile { path }
            | RollbackAction::DeleteDirectoryIfEmpty { path } => {
                validate_relative_path(path, false, &format!("{prefix}.path"), report);
            }
            RollbackAction::RestoreBackup { original_path } => {
                validate_relative_path(
                    original_path,
                    false,
                    &format!("{prefix}.original_path"),
                    report,
                );
            }
            RollbackAction::RemoveRegistryCompatFlags { hive, target_executable } => {
                validate_relative_path(
                    target_executable,
                    false,
                    &format!("{prefix}.target_executable"),
                    report,
                );
                if *hive == RegistryHive::Hklm {
                    report.push_warning(
                        format!("{prefix}.hive"),
                        ValidationCode::HklmRegistryWrite,
                        "rollback action targets HKLM registry hive; prefer `hkcu`",
                    );
                }
            }
        }
    }
}

fn validate_https_url(raw: &str, field: &str, report: &mut ValidationReport) {
    match Url::parse(raw) {
        Ok(parsed) if parsed.scheme() == "https" && parsed.has_host() => {}
        Ok(parsed) => {
            report.push_error(
                field,
                ValidationCode::InsecureOrInvalidUrl,
                format!(
                    "URL `{raw}` must use `https://` scheme with a valid host (found `{}`)",
                    parsed.scheme()
                ),
            );
        }
        Err(err) => {
            report.push_error(
                field,
                ValidationCode::InsecureOrInvalidUrl,
                format!("invalid URL `{raw}`: {err}"),
            );
        }
    }
}

fn validate_hex_digest(
    value: &str,
    expected_len: usize,
    algo: &str,
    field: &str,
    report: &mut ValidationReport,
) {
    let valid = value.len() == expected_len
        && value.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !valid {
        report.push_error(
            field,
            ValidationCode::InvalidHexDigest,
            format!(
                "{algo} digest `{value}` must be exactly {expected_len} lowercase hex characters"
            ),
        );
    }
}

fn validate_glob_pattern(pattern: &str, field: &str, report: &mut ValidationReport) {
    if pattern.trim().is_empty()
        || pattern.starts_with('/')
        || pattern.starts_with('\\')
        || pattern.contains("..")
        || pattern.contains(':')
        || pattern.chars().any(char::is_control)
    {
        report.push_error(
            field,
            ValidationCode::UnsafeRelativePath,
            format!("unsafe archive include pattern `{pattern}`"),
        );
    }
}

fn validate_relative_path(path: &str, allow_dot: bool, field: &str, report: &mut ValidationReport) {
    if let Some(reason) = check_relative_path(path, allow_dot) {
        report.push_error(
            field,
            ValidationCode::UnsafeRelativePath,
            format!("unsafe relative path `{path}`: {reason}"),
        );
    }
}

fn check_relative_path(path: &str, allow_dot: bool) -> Option<&'static str> {
    if path.is_empty() {
        return Some("path must not be empty");
    }
    if path == "." {
        return if allow_dot {
            None
        } else {
            Some("`.` is only permitted for directory target fields")
        };
    }
    if path.chars().any(|c| c == '\0' || c.is_control()) {
        return Some("path contains NUL or control characters");
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Some("path must be relative, not rooted or UNC");
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Some("Windows drive letter prefixes are forbidden");
    }

    for segment in path.split(['/', '\\']) {
        if segment.is_empty() {
            return Some("path contains empty or trailing separator segments");
        }
        if segment == "." || segment == ".." {
            return Some("`.` and `..` traversal segments are forbidden");
        }
        if segment.ends_with(' ') || segment.ends_with('.') {
            return Some("path segments must not end with whitespace or `.`");
        }
        if segment.chars().any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*')) {
            return Some("path contains reserved Windows filename characters");
        }
        let stem = segment.split('.').next().unwrap_or(segment);
        if is_windows_reserved_device(stem) {
            return Some("path contains a reserved Windows DOS device name");
        }
    }

    None
}

fn is_windows_reserved_device(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

fn has_allowed_extension(path: &str, extensions: &[&str]) -> bool {
    let lower = path.to_ascii_lowercase();
    extensions.iter().any(|ext| lower.ends_with(ext))
}

fn is_valid_slug(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return false;
    }
    let first_ok = matches!(bytes[0], b'a'..=b'z' | b'0'..=b'9');
    let rest_ok =
        bytes[1..].iter().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'));
    first_ok && rest_ok
}

fn is_valid_semver(version: &str) -> bool {
    let without_build =
        version.split_once('+').map_or(
            version,
            |(main, build)| {
                if build.is_empty() {
                    ""
                } else {
                    main
                }
            },
        );
    if without_build.is_empty() {
        return false;
    }
    let core =
        without_build.split_once('-').map_or(
            without_build,
            |(core, pre)| {
                if pre.is_empty() {
                    ""
                } else {
                    core
                }
            },
        );
    let mut parts = core.split('.');
    let Some(major) = parts.next() else { return false };
    let Some(minor) = parts.next() else { return false };
    let Some(patch) = parts.next() else { return false };
    if parts.next().is_some() {
        return false;
    }
    [major, minor, patch].into_iter().all(is_valid_semver_num)
}

fn is_valid_semver_num(part: &str) -> bool {
    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    part == "0" || !part.starts_with('0')
}

fn is_valid_iso_date(date: &str) -> bool {
    let bytes = date.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let Ok(year) = date[0..4].parse::<u16>() else { return false };
    let Ok(month) = date[5..7].parse::<u8>() else { return false };
    let Ok(day) = date[8..10].parse::<u8>() else { return false };

    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0);
            if leap {
                29
            } else {
                28
            }
        }
        _ => return false,
    };
    (1..=max_day).contains(&day)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::schema::{
        BinaryArchitecture, Distribution, ExecutableFingerprint, GameRelease, HealthStatus,
        IssueCategory, IssueSeverity, KnownIssue, PartialHash, RecipeMetadata, RuntimeRequirements,
        SandboxConfig,
    };
    use crate::steps::{
        AppCompatFlag, ArchiveFormat, CpuAffinityMode, IniEncoding, LineEnding, OsFilter,
        RollbackStrategy,
    };
    use crate::wrapper::{DgVoodooApi, TargetArch, WrapperDeployment, WrapperKind};

    fn sample_sha256(fill: char) -> String {
        std::iter::repeat_n(fill, 64).collect()
    }

    fn sample_valid_recipe() -> GameRecipe {
        let mut ini_sections = BTreeMap::new();
        let mut main_sec = BTreeMap::new();
        main_sec.insert("ResX".to_owned(), IniValue::Integer(1920));
        ini_sections.insert("MAIN".to_owned(), main_sec);

        GameRecipe {
            schema_version: CURRENT_SCHEMA_VERSION,
            game_id: "sample-game-2004".to_owned(),
            title: "Sample Legacy Game".to_owned(),
            release_year: 2004,
            developer: Some("Studio".to_owned()),
            publisher: Some("Publisher".to_owned()),
            metadata: RecipeMetadata {
                recipe_version: "1.0.0".to_owned(),
                authors: vec!["Maintainer".to_owned()],
                license: "MIT".to_owned(),
                updated_at: "2026-11-22".to_owned(),
                pcgamingwiki_url: Some("https://www.pcgamingwiki.com/wiki/Home".to_owned()),
                steam_app_id: None,
                steamgriddb_id: None,
                cover_art_url: None,
                required_game_version: None,
                incompatible_releases: Vec::new(),
                notes: None,
            },
            baseline_health: HealthStatus::NeedsWrapper,
            post_fix_health: HealthStatus::ReadyToPlay,
            releases: vec![GameRelease {
                release_id: "retail-v1.0".to_owned(),
                label: "Retail DVD v1.0".to_owned(),
                distribution: Distribution::RetailDvd,
                version: "1.0".to_owned(),
                region: Some("Global".to_owned()),
                executable_path: "bin/game.exe".to_owned(),
                fingerprint: ExecutableFingerprint {
                    file_size_bytes: Some(1_048_576),
                    partial_sha256: Some(PartialHash {
                        offset: 0,
                        length: 65_536,
                        sha256: sample_sha256('a'),
                    }),
                    sha256: Some(sample_sha256('b')),
                    md5: None,
                    verification_status: HashVerificationStatus::Verified,
                },
                sentinel_files: vec!["data/pack.bin".to_owned()],
            }],
            known_issues: vec![KnownIssue {
                id: "dx8-shadows".to_owned(),
                category: IssueCategory::GraphicsApi,
                severity: IssueSeverity::Major,
                summary: "Broken DX8 shadows".to_owned(),
                details: "Requires dgVoodoo 2 DX8 wrapper.".to_owned(),
                fixed_by_fixes: vec!["dgvoodoo2_wrapper".to_owned()],
                user_action_required: None,
            }],
            runtime_requirements: RuntimeRequirements {
                architecture: BinaryArchitecture::Win32,
                requires_16bit_emulator: false,
                native_graphics_api: vec!["Direct3D 8".to_owned()],
                target_graphics_api: Some("Direct3D 11".to_owned()),
                requires_admin: false,
            },
            sandbox: Some(SandboxConfig {
                enabled_by_default: true,
                sandbox_root: ".ogsc_sandbox".to_owned(),
                redirect_documents: false,
                redirect_appdata_local: false,
                redirect_appdata_roaming: false,
                known_save_subpaths: vec!["saves".to_owned()],
            }),
            fixes: vec![
                FixOption {
                    id: "widescreen_fix".to_owned(),
                    title: "Widescreen Fix".to_owned(),
                    description: "Downloads open-source MIT widescreen ASI and writes INI."
                        .to_owned(),
                    recommended: true,
                    applies_to_releases: Vec::new(),
                    steps: vec![
                        PatchStep {
                            id: "download-fix".to_owned(),
                            name: "Download Widescreen Fix".to_owned(),
                            description: "Downloads open-source MIT fix.".to_owned(),
                            applies_to_releases: Vec::new(),
                            os_filter: Some(OsFilter {
                                min_windows_build: Some(19041),
                                max_windows_build: None,
                            }),
                            action: PatchAction::DownloadVerifiedZip {
                                url:
                                    "https://github.com/example/fix/releases/download/v1.0/fix.zip"
                                        .to_owned(),
                                mirror_urls: Vec::new(),
                                sha256: sample_sha256('c'),
                                verification_status: HashVerificationStatus::Verified,
                                size_bytes: Some(204_800),
                                archive_format: ArchiveFormat::Zip,
                                extract_to: ".".to_owned(),
                                include_patterns: vec!["scripts/*".to_owned()],
                                strip_components: 0,
                                overwrite_existing: true,
                                license: "MIT".to_owned(),
                                source_repo: Some("https://github.com/example/fix".to_owned()),
                            },
                        },
                        PatchStep {
                            id: "extract-asi".to_owned(),
                            name: "Extract ASI plugin".to_owned(),
                            description: "Extracts ASI plugin from cached archive.".to_owned(),
                            applies_to_releases: vec!["retail-v1.0".to_owned()],
                            os_filter: None,
                            action: PatchAction::ExtractDll {
                                source: DllSource::CachedArchive {
                                    download_step_id: "download-fix".to_owned(),
                                    archive_member: "scripts/fix.asi".to_owned(),
                                },
                                destination_path: "scripts/fix.asi".to_owned(),
                                expected_sha256: Some(sample_sha256('d')),
                                verification_status: HashVerificationStatus::Verified,
                                backup_original: true,
                            },
                        },
                        PatchStep {
                            id: "write-ini".to_owned(),
                            name: "Configure Fix INI".to_owned(),
                            description: "Writes desktop resolution.".to_owned(),
                            applies_to_releases: Vec::new(),
                            os_filter: None,
                            action: PatchAction::WriteIniConfig {
                                target_file: "scripts/fix.ini".to_owned(),
                                create_if_missing: true,
                                encoding: IniEncoding::Utf8,
                                line_ending: LineEnding::Crlf,
                                sections: ini_sections,
                            },
                        },
                    ],
                },
                FixOption {
                    id: "dgvoodoo2_wrapper".to_owned(),
                    title: "dgVoodoo 2 Wrapper".to_owned(),
                    description: "Deploys dgVoodoo 2 and sets HKCU HighDPIAware.".to_owned(),
                    recommended: true,
                    applies_to_releases: Vec::new(),
                    steps: vec![
                        PatchStep {
                            id: "deploy-dgvoodoo".to_owned(),
                            name: "Deploy dgVoodoo 2".to_owned(),
                            description: "Wraps Direct3D 8 to Direct3D 11.".to_owned(),
                            applies_to_releases: Vec::new(),
                            os_filter: None,
                            action: PatchAction::DeployWrapper(WrapperDeployment {
                                wrapper: WrapperKind::DgVoodoo2 {
                                    api_targets: vec![DgVoodooApi::D3d8],
                                    arch: TargetArch::X86,
                                    config_overrides: BTreeMap::new(),
                                },
                                min_toolkit_version: Some("2.82.3".to_owned()),
                                target_dir: "bin".to_owned(),
                                dll_chain_renames: Vec::new(),
                            }),
                        },
                        PatchStep {
                            id: "compat-flags".to_owned(),
                            name: "Set HKCU Compatibility Flags".to_owned(),
                            description: "Applies HighDPIAware flag.".to_owned(),
                            applies_to_releases: Vec::new(),
                            os_filter: None,
                            action: PatchAction::SetRegistryCompatFlags {
                                hive: RegistryHive::Hkcu,
                                target_executable: "bin/game.exe".to_owned(),
                                flags: vec![AppCompatFlag::HighDpiAware],
                                merge_with_existing: true,
                            },
                        },
                    ],
                },
            ],
            launch: LaunchConfig {
                default_executable: "bin/game.exe".to_owned(),
                release_executable_overrides: BTreeMap::new(),
                fix_executable_overrides: BTreeMap::new(),
                working_directory: Some("bin".to_owned()),
                arguments: Vec::new(),
                environment: BTreeMap::new(),
                emulator_hook: None,
            },
            rollback: RollbackPlan {
                strategy: RollbackStrategy::TransactionalAutomatic,
                backup_directory: ".ogsc_backup".to_owned(),
                preserve_user_saves: true,
                restore_registry_values: true,
                extra_cleanup_paths: Vec::new(),
                custom_actions: Vec::new(),
            },
        }
    }

    #[test]
    fn valid_recipe_passes_strict_validation() {
        let recipe = sample_valid_recipe();
        let report = recipe.validate();
        assert!(report.is_valid(), "expected no errors, got: {report}");
        assert!(!report.has_warnings(), "expected no warnings, got: {report}");
        assert!(report.into_result(true).is_ok());
    }

    #[test]
    fn rejects_unsafe_paths_and_reserved_windows_devices() {
        let bad_paths = [
            "",
            "../evil.dll",
            "scripts/../../evil.dll",
            "/absolute/path.dll",
            r"\\server\share\evil.dll",
            r"C:\Windows\System32\d3d9.dll",
            "scripts/CON.ini",
            "NUL",
            "scripts/LPT1.dll",
            "trailing_dot.",
        ];
        for bad in bad_paths {
            let mut recipe = sample_valid_recipe();
            recipe.launch.default_executable = bad.to_owned();
            let report = recipe.validate();
            assert!(
                report.errors.iter().any(|e| e.code == ValidationCode::UnsafeRelativePath),
                "path `{bad}` should be rejected as UnsafeRelativePath, got: {report}"
            );
        }
    }

    #[test]
    fn rejects_http_url_and_bad_digests() {
        let mut recipe = sample_valid_recipe();
        if let PatchAction::DownloadVerifiedZip { url, sha256, .. } =
            &mut recipe.fixes[0].steps[0].action
        {
            *url = "http://insecure.example.com/fix.zip".to_owned();
            *sha256 = "DEADBEAF".to_owned();
        }
        let report = recipe.validate();
        assert!(report.errors.iter().any(|e| e.code == ValidationCode::InsecureOrInvalidUrl));
        assert!(report.errors.iter().any(|e| e.code == ValidationCode::InvalidHexDigest));
    }

    #[test]
    fn rejects_out_of_order_and_cross_fix_cached_archive_references() {
        let mut out_of_order = sample_valid_recipe();
        out_of_order.fixes[0].steps.swap(0, 1);
        let report = out_of_order.validate();
        assert!(report.errors.iter().any(|e| e.code == ValidationCode::BrokenCrossReference));

        let mut cross_fix = sample_valid_recipe();
        let extract_step = cross_fix.fixes[0].steps.remove(1);
        cross_fix.fixes[1].steps.push(extract_step);
        let cross_report = cross_fix.validate();
        assert!(
            cross_report.errors.iter().any(|e| e.code == ValidationCode::BrokenCrossReference),
            "cached_archive must not reference a download step from another FixOption"
        );
    }

    #[test]
    fn rejects_empty_fix_steps() {
        let mut recipe = sample_valid_recipe();
        recipe.fixes[1].steps.clear();
        let report = recipe.validate();
        assert!(report.errors.iter().any(|e| e.code == ValidationCode::EmptyRequiredField));
    }

    #[test]
    fn rejects_identical_bsdiff_hashes_and_empty_ini_section() {
        let mut recipe = sample_valid_recipe();
        recipe.fixes[1].steps.push(PatchStep {
            id: "bsdiff-step".to_owned(),
            name: "Patch binary".to_owned(),
            description: "Applies binary diff.".to_owned(),
            applies_to_releases: Vec::new(),
            os_filter: None,
            action: PatchAction::ApplyBinaryBsdiffPatch {
                target_file: "bin/game.exe".to_owned(),
                patch_source: BinaryPatchSource::LocalManifestRelative {
                    path: "patches/fix.bsdiff".to_owned(),
                    sha256: sample_sha256('e'),
                },
                expected_original_sha256: sample_sha256('f'),
                expected_patched_sha256: sample_sha256('f'),
                verification_status: HashVerificationStatus::Verified,
                backup_original: true,
            },
        });
        if let PatchAction::WriteIniConfig { sections, .. } = &mut recipe.fixes[0].steps[2].action {
            sections.insert("EMPTY".to_owned(), BTreeMap::new());
        }
        let report = recipe.validate();
        assert!(report.errors.iter().any(|e| e.code == ValidationCode::InvalidActionConfiguration));
        assert!(report.errors.iter().any(|e| e.code == ValidationCode::EmptyRequiredField));
    }

    #[test]
    fn warns_on_template_hashes_and_hklm_and_fails_strict_mode() {
        let mut recipe = sample_valid_recipe();
        recipe.releases[0].fingerprint.verification_status =
            HashVerificationStatus::UnverifiedTemplate;
        if let PatchAction::SetRegistryCompatFlags { hive, .. } =
            &mut recipe.fixes[1].steps[1].action
        {
            *hive = RegistryHive::Hklm;
        }
        let report = recipe.validate();
        assert!(report.is_valid());
        assert!(report.warnings.iter().any(|w| w.code == ValidationCode::UnverifiedTemplateHash));
        assert!(report.warnings.iter().any(|w| w.code == ValidationCode::HklmRegistryWrite));
        assert!(report.clone().into_result(false).is_ok());
        assert!(report.into_result(true).is_err());
    }

    #[test]
    fn rejects_broken_16bit_emulator_and_os_filter_bounds() {
        let mut recipe = sample_valid_recipe();
        recipe.runtime_requirements.requires_16bit_emulator = true;
        recipe.fixes[0].steps[0].os_filter =
            Some(OsFilter { min_windows_build: Some(26100), max_windows_build: Some(22000) });
        let report = recipe.validate();
        assert!(report.errors.iter().any(|e| e.code == ValidationCode::BrokenCrossReference));
        assert!(report.errors.iter().any(|e| e.code == ValidationCode::OutOfRange));
    }

    #[test]
    fn rejects_unknown_release_in_incompatible_releases() {
        let mut recipe = sample_valid_recipe();
        recipe.metadata.incompatible_releases.push("does-not-exist".to_owned());
        let report = recipe.validate();
        assert!(report.errors.iter().any(|e| {
            e.code == ValidationCode::BrokenCrossReference
                && e.field == "metadata.incompatible_releases[0]"
        }));
    }

    #[test]
    fn cpu_affinity_mask_computations_are_accurate() {
        assert_eq!(CpuAffinityMode::SingleCore.to_affinity_mask(16), 0x1);
        assert_eq!(CpuAffinityMode::MaxLogicalCores { count: 4 }.to_affinity_mask(16), 0xF);
        assert_eq!(CpuAffinityMode::MaxLogicalCores { count: 8 }.to_affinity_mask(4), 0xF);
        assert_eq!(
            CpuAffinityMode::PhysicalCoresOnly { max_cores: Some(2) }.to_affinity_mask(8),
            0b0101
        );
        assert_eq!(CpuAffinityMode::ExplicitMask { mask: 0x0A }.to_affinity_mask(16), 0x0A);
        assert_eq!(CpuAffinityMode::ExplicitMask { mask: 0x100 }.to_affinity_mask(4), 0x01);
    }

    #[test]
    fn app_compat_tokens_match_windows_registry_strings() {
        assert_eq!(AppCompatFlag::WinXpSp3.to_windows_token(), "WINXPSP3");
        assert_eq!(AppCompatFlag::Color8Bit.to_windows_token(), "256COLOR");
        assert_eq!(
            AppCompatFlag::DisableFullscreenOptimizations.to_windows_token(),
            "DISABLEDXMAXIMIZEDWINDOWEDMODE"
        );
        assert_eq!(AppCompatFlag::HighDpiAware.to_windows_token(), "HIGHDPIAWARE");
        assert_eq!(
            AppCompatFlag::Custom("DWM8And16BitMitigation".to_owned()).to_windows_token(),
            "DWM8And16BitMitigation"
        );
    }
}

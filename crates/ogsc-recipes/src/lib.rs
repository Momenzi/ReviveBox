//! Declarative compatibility recipe schema, security validator, and loader for **Old Games – Second Chance**.
//!
//! Every supported legacy PC game (1995–2012) is described by a pure-data [`GameRecipe`] manifest
//! validated against strict path-safety, HTTPS-download, cross-reference, and cryptographic-digest
//! invariants before any scanner or patcher touches the user's filesystem.

#![forbid(unsafe_code)]

pub mod error;
pub mod loader;
pub mod schema;
pub mod steps;
pub mod validation;
pub mod wrapper;

pub use error::RecipeError;
pub use loader::{LoadedDatabase, RecipeDatabase, RecipeWarningRecord, ReleaseRef};
pub use schema::{
    BinaryArchitecture, Distribution, EmulatorKind, EmulatorLaunchHook, ExecutableFingerprint,
    GameRecipe, GameRelease, HashVerificationStatus, HealthStatus, IssueCategory, IssueSeverity,
    KnownIssue, LaunchConfig, PartialHash, RecipeMetadata, RuntimeRequirements, SandboxConfig,
    CURRENT_SCHEMA_VERSION,
};
pub use steps::{
    AffinityApplyMethod, AppCompatFlag, ArchiveFormat, BinaryPatchSource, CpuAffinityMode,
    DllSource, DynamicIniValue, DynamicValueKind, FixOption, IniEncoding, IniValue, LineEnding,
    OsFilter, PatchAction, PatchStep, ProcessPriority, RegistryHive, RollbackAction, RollbackPlan,
    RollbackStrategy,
};
pub use validation::{ValidationCode, ValidationIssue, ValidationReport};
pub use wrapper::{
    DgVoodooApi, DllChainRule, LegacyOsProfile, TargetArch, WineD3dRenderer, WrapperDeployment,
    WrapperKind,
};

/// Generates the Draft 2020-12 JSON Schema for [`GameRecipe`].
pub fn generate_json_schema() -> schemars::Schema {
    schemars::schema_for!(GameRecipe)
}

/// Serializes the Draft 2020-12 JSON Schema for [`GameRecipe`] as pretty-printed JSON.
pub fn generate_json_schema_pretty() -> Result<String, RecipeError> {
    let schema = generate_json_schema();
    let mut json = serde_json::to_string_pretty(&schema)
        .map_err(|source| RecipeError::SerializeJson { source })?;
    json.push('\n');
    Ok(json)
}

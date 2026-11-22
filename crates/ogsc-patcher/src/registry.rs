//! Windows Registry `AppCompatFlags\Layers` backend, flag merging, and rollback reversal.

use std::collections::BTreeMap;
use std::path::Path;

use ogsc_recipes::{AppCompatFlag, RegistryHive};
use ogsc_scanner::RegistryChangeRecord;

use crate::error::PatcherError;

/// Canonical Windows Registry subkey holding per-executable compatibility layers.
pub const APP_COMPAT_LAYERS_KEY: &str =
    r"Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers";

/// Abstraction over Windows Registry operations enabling deterministic unit & e2e tests.
pub trait RegistryBackend {
    /// Reads a string value under `hive\\key_path\\value_name`, returning `Ok(None)` if absent.
    fn read_string(
        &self,
        hive: RegistryHive,
        key_path: &str,
        value_name: &str,
    ) -> Result<Option<String>, PatcherError>;

    /// Creates or overwrites a `REG_SZ` string value at `hive\\key_path\\value_name`.
    fn write_string(
        &mut self,
        hive: RegistryHive,
        key_path: &str,
        value_name: &str,
        value: &str,
    ) -> Result<(), PatcherError>;

    /// Deletes a value under `hive\\key_path\\value_name` (succeeding if already absent).
    fn delete_value(
        &mut self,
        hive: RegistryHive,
        key_path: &str,
        value_name: &str,
    ) -> Result<(), PatcherError>;
}

/// Real Windows Registry backend backed by `winreg` (`HKCU` / `HKLM`).
#[cfg(windows)]
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsRegistryBackend;

#[cfg(windows)]
impl WindowsRegistryBackend {
    fn root_hkey(hive: RegistryHive) -> winreg::HKEY {
        match hive {
            RegistryHive::Hkcu => winreg::enums::HKEY_CURRENT_USER,
            RegistryHive::Hklm => winreg::enums::HKEY_LOCAL_MACHINE,
        }
    }
}

#[cfg(windows)]
impl RegistryBackend for WindowsRegistryBackend {
    fn read_string(
        &self,
        hive: RegistryHive,
        key_path: &str,
        value_name: &str,
    ) -> Result<Option<String>, PatcherError> {
        let root = winreg::RegKey::predef(Self::root_hkey(hive));
        let subkey = match root.open_subkey(key_path) {
            Ok(key) => key,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(PatcherError::RegistryError {
                    key_path: key_path.to_owned(),
                    value_name: value_name.to_owned(),
                    reason: err.to_string(),
                });
            }
        };

        match subkey.get_value::<String, _>(value_name) {
            Ok(val) => Ok(Some(val)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(PatcherError::RegistryError {
                key_path: key_path.to_owned(),
                value_name: value_name.to_owned(),
                reason: err.to_string(),
            }),
        }
    }

    fn write_string(
        &mut self,
        hive: RegistryHive,
        key_path: &str,
        value_name: &str,
        value: &str,
    ) -> Result<(), PatcherError> {
        let root = winreg::RegKey::predef(Self::root_hkey(hive));
        let (subkey, _) =
            root.create_subkey(key_path).map_err(|err| PatcherError::RegistryError {
                key_path: key_path.to_owned(),
                value_name: value_name.to_owned(),
                reason: err.to_string(),
            })?;
        subkey.set_value(value_name, &value).map_err(|err| PatcherError::RegistryError {
            key_path: key_path.to_owned(),
            value_name: value_name.to_owned(),
            reason: err.to_string(),
        })
    }

    fn delete_value(
        &mut self,
        hive: RegistryHive,
        key_path: &str,
        value_name: &str,
    ) -> Result<(), PatcherError> {
        let root = winreg::RegKey::predef(Self::root_hkey(hive));
        let subkey = match root
            .open_subkey_with_flags(key_path, winreg::enums::KEY_READ | winreg::enums::KEY_WRITE)
        {
            Ok(key) => key,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(PatcherError::RegistryError {
                    key_path: key_path.to_owned(),
                    value_name: value_name.to_owned(),
                    reason: err.to_string(),
                });
            }
        };

        match subkey.delete_value(value_name) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(PatcherError::RegistryError {
                key_path: key_path.to_owned(),
                value_name: value_name.to_owned(),
                reason: err.to_string(),
            }),
        }
    }
}

/// Pure-Rust in-memory registry backend used for hermetic unit and integration testing.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MemoryRegistryBackend {
    values: BTreeMap<(u8, String, String), String>,
}

impl MemoryRegistryBackend {
    /// Creates an empty [`MemoryRegistryBackend`].
    pub fn new() -> Self {
        Self::default()
    }

    fn hive_id(hive: RegistryHive) -> u8 {
        match hive {
            RegistryHive::Hkcu => 0,
            RegistryHive::Hklm => 1,
        }
    }

    fn key_tuple(hive: RegistryHive, key_path: &str, value_name: &str) -> (u8, String, String) {
        (Self::hive_id(hive), key_path.to_ascii_lowercase(), value_name.to_ascii_lowercase())
    }
}

impl RegistryBackend for MemoryRegistryBackend {
    fn read_string(
        &self,
        hive: RegistryHive,
        key_path: &str,
        value_name: &str,
    ) -> Result<Option<String>, PatcherError> {
        Ok(self.values.get(&Self::key_tuple(hive, key_path, value_name)).cloned())
    }

    fn write_string(
        &mut self,
        hive: RegistryHive,
        key_path: &str,
        value_name: &str,
        value: &str,
    ) -> Result<(), PatcherError> {
        self.values.insert(Self::key_tuple(hive, key_path, value_name), value.to_owned());
        Ok(())
    }

    fn delete_value(
        &mut self,
        hive: RegistryHive,
        key_path: &str,
        value_name: &str,
    ) -> Result<(), PatcherError> {
        self.values.remove(&Self::key_tuple(hive, key_path, value_name));
        Ok(())
    }
}

/// Computes the canonical `AppCompatFlags\Layers` string (`~ TOKEN1 TOKEN2`), deduplicating case-insensitively.
pub fn merge_compat_flags(
    existing: Option<&str>,
    flags: &[AppCompatFlag],
    merge_with_existing: bool,
) -> String {
    let mut tokens: Vec<String> = Vec::new();

    if merge_with_existing {
        if let Some(prev) = existing {
            for tok in prev.split_whitespace() {
                if tok == "~" {
                    continue;
                }
                if !tokens.iter().any(|t| t.eq_ignore_ascii_case(tok)) {
                    tokens.push(tok.to_owned());
                }
            }
        }
    }

    for flag in flags {
        let tok = flag.to_windows_token();
        if !tokens.iter().any(|t| t.eq_ignore_ascii_case(tok)) {
            tokens.push(tok.to_owned());
        }
    }

    if tokens.is_empty() {
        String::new()
    } else {
        format!("~ {}", tokens.join(" "))
    }
}

/// Reads the prior registry value, applies merged Windows AppCompat flags, and returns the rollback record.
pub fn apply_compat_flags(
    backend: &mut dyn RegistryBackend,
    hive: RegistryHive,
    exe_abs_path: &Path,
    flags: &[AppCompatFlag],
    merge_with_existing: bool,
) -> Result<(RegistryChangeRecord, String), PatcherError> {
    let value_name = exe_abs_path.to_string_lossy().into_owned();
    let previous_value = backend.read_string(hive, APP_COMPAT_LAYERS_KEY, &value_name)?;
    let new_value = merge_compat_flags(previous_value.as_deref(), flags, merge_with_existing);

    backend.write_string(hive, APP_COMPAT_LAYERS_KEY, &value_name, &new_value)?;
    let record = RegistryChangeRecord {
        hive,
        key_path: APP_COMPAT_LAYERS_KEY.to_owned(),
        value_name,
        previous_value,
    };
    Ok((record, new_value))
}

/// Reverts captured [`RegistryChangeRecord`] entries in reverse chronological order.
pub fn revert_registry_changes(
    backend: &mut dyn RegistryBackend,
    records: &[RegistryChangeRecord],
) -> Result<usize, PatcherError> {
    let mut reverted = 0_usize;
    for rec in records.iter().rev() {
        match &rec.previous_value {
            Some(prev) => backend.write_string(rec.hive, &rec.key_path, &rec.value_name, prev)?,
            None => backend.delete_value(rec.hive, &rec.key_path, &rec.value_name)?,
        }
        reverted += 1;
    }
    Ok(reverted)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn merges_deduplicates_and_reverts_compat_flags_in_memory() {
        let mut reg = MemoryRegistryBackend::new();
        let exe = PathBuf::from(r"C:\Games\NFS\speed.exe");
        let exe_name = exe.to_string_lossy().into_owned();

        reg.write_string(
            RegistryHive::Hkcu,
            APP_COMPAT_LAYERS_KEY,
            &exe_name,
            "~ RUNASADMIN HIGHDPIAWARE",
        )
        .expect("seed prior value");

        let (rec1, val1) = apply_compat_flags(
            &mut reg,
            RegistryHive::Hkcu,
            &exe,
            &[AppCompatFlag::HighDpiAware, AppCompatFlag::DisableFullscreenOptimizations],
            true,
        )
        .expect("apply merged flags");

        assert_eq!(val1, "~ RUNASADMIN HIGHDPIAWARE DISABLEDXMAXIMIZEDWINDOWEDMODE");
        assert_eq!(rec1.previous_value.as_deref(), Some("~ RUNASADMIN HIGHDPIAWARE"));

        let reverted = revert_registry_changes(&mut reg, &[rec1]).expect("revert");
        assert_eq!(reverted, 1);
        assert_eq!(
            reg.read_string(RegistryHive::Hkcu, APP_COMPAT_LAYERS_KEY, &exe_name).expect("read"),
            Some("~ RUNASADMIN HIGHDPIAWARE".to_owned())
        );
    }

    #[test]
    fn deletes_registry_value_on_rollback_when_none_existed_before() {
        let mut reg = MemoryRegistryBackend::new();
        let exe = PathBuf::from(r"C:\Games\SC2\SplinterCell2.exe");
        let exe_name = exe.to_string_lossy().into_owned();

        let (rec, val) = apply_compat_flags(
            &mut reg,
            RegistryHive::Hkcu,
            &exe,
            &[AppCompatFlag::WinXpSp3],
            false,
        )
        .expect("apply new flag");
        assert_eq!(val, "~ WINXPSP3");
        assert_eq!(rec.previous_value, None);

        revert_registry_changes(&mut reg, &[rec]).expect("revert");
        assert_eq!(
            reg.read_string(RegistryHive::Hkcu, APP_COMPAT_LAYERS_KEY, &exe_name).expect("read"),
            None
        );
    }
}

//! The App page's launcher flags and saved hardware hints, kept in the registry file.

use super::*;
use openrigcue_config::import::value::LegacyValue;

impl ConfigStore {
    /// Records a text-valued hardware hint when the settings are saved, independently
    /// of the profile transaction. An unsuccessful write returns
    /// false, which the save ignores.
    pub(crate) fn record_saved_hardware_hint(&self, key: &str, value: &str) -> bool {
        let result: Result<(), StoreError> = (|| {
            self.persist_first_run()?;
            if let Some(dir) = &self.dir {
                let path = dir.registry_file();
                openrigcue_config::import::registry::set_item(dir.root(), key, &LegacyValue::from(value))
                    .map_err(|error| StoreError::Parse { path, message: error.to_string() })?;
            }
            Ok(())
        })();
        result.is_ok()
    }

    fn launcher_setting(&self, key: &str) -> Result<bool, StoreError> {
        match key {
            "StartWithWindows" => Ok(self.settings().app.start_with_login),
            "ShowSplashScreen" => Ok(self.settings().app.show_splash_screen),
            _ => Err(StoreError::Parse {
                path: self.dir.as_ref().map_or_else(|| "registry.db".into(), ConfigDir::registry_file),
                message: format!("unknown launcher flag {key:?}"),
            }),
        }
    }

    pub(crate) fn launcher_flag(&self, key: &str) -> Result<Option<LegacyValue>, StoreError> {
        let cached = self.launcher_setting(key)?;
        if let Some(dir) = &self.dir {
            if self.native_files_initialized() {
                let path = dir.registry_file();
                return openrigcue_config::import::registry::read_item(dir.root(), key)
                    .map_err(|error| StoreError::Parse { path, message: error.to_string() });
            }
        }
        // Memory/fresh-folder fixtures represent the completed first-run flags.
        Ok(Some(LegacyValue::from(if cached { "1" } else { "0" })))
    }

    /// Writes the flag immediately, without reading the stored value, touching the
    /// INI or committing any profile transaction.
    pub(crate) fn write_launcher_flag(&mut self, key: &str, enabled: bool) -> Result<(), StoreError> {
        let previous = self.launcher_setting(key)?;
        self.persist_first_run()?;
        if let Some(dir) = &self.dir {
            let path = dir.registry_file();
            openrigcue_config::import::registry::set_item(
                dir.root(),
                key,
                &LegacyValue::from(if enabled { "1" } else { "0" }),
            )
            .map_err(|error| StoreError::Parse { path, message: error.to_string() })?;
        }
        if previous != enabled {
            self.update(|configuration| match key {
                "StartWithWindows" => configuration.settings.app.start_with_login = enabled,
                "ShowSplashScreen" => configuration.settings.app.show_splash_screen = enabled,
                _ => unreachable!("validated launcher flag"),
            });
        }
        Ok(())
    }
}

//! Configuration, per-game tunes, profiles, game catalog and game detection.

pub mod catalog;
pub mod detect;
pub mod filecmp;
pub mod import;
pub mod patch;
pub mod profiles;
pub mod settings;
pub mod synced_profiles;
pub mod tunes;

pub use settings::paths::StoreError;

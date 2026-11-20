//! The template data openrigcue ships (`data/srs/`, `data/tunes/`), parsed into legacy values.
//!
//! These are the files a fresh installation starts from — `template.ini` (which
//! fills a missing `config.ini`), the template `profile.db` (also the
//! template `metadata.db`, whose `shakers`/`joystick`/notification tables are absent), and
//! the shaker layouts — and the read-only tables the migration steps copy from.

use std::sync::OnceLock;

use super::migrate::{LegacyLayouts, LegacyState, MigrationSources, Platform};
use super::value::{Dict, LegacyValue};
use crate::catalog::registry::Registry;

/// Every shipped dataset, parsed once.
#[derive(Debug)]
pub struct ShippedData {
    /// `template.ini`, parsed (`config_file_version` 84,
    /// `first_time_installation = yes`).
    pub template_config: Dict,
    /// The template `profile.db` (table `unnamed`).
    pub template_profiles: Dict,
    /// The factory tune of every game.
    pub master_tunes: Dict,
    /// The expert tune sets.
    pub expert_tunes: Dict,
    /// The flight shaker levels of each layout.
    pub flight_shaker_levels: Dict,
    /// The shipped shaker layouts.
    pub shaker_layouts: Dict,
    /// The `SRS UShake6 Enhanced` layout added by migration v345.
    pub ushake6_enhanced: Dict,
}

fn parse_dict(name: &str, text: &str) -> Dict {
    let json: serde_json::Value = serde_json::from_str(text).unwrap_or_else(|e| panic!("{name} is valid JSON: {e}"));
    match LegacyValue::from(json) {
        LegacyValue::Dict(d) => d,
        _ => panic!("{name} is a JSON object"),
    }
}

impl ShippedData {
    pub fn get() -> &'static ShippedData {
        static DATA: OnceLock<ShippedData> = OnceLock::new();
        DATA.get_or_init(|| ShippedData {
            template_config: super::configobj::parse(include_str!("../../data/srs/template.ini"))
                .unwrap_or_else(|e| panic!("template.ini is valid ConfigObj: {e}")),
            template_profiles: parse_dict(
                "template_profiles.json",
                include_str!("../../data/srs/template_profiles.json"),
            ),
            master_tunes: parse_dict("defaults.json", include_str!("../../data/tunes/defaults.json")),
            expert_tunes: parse_dict("expert_profiles.json", include_str!("../../data/tunes/expert_profiles.json")),
            flight_shaker_levels: parse_dict(
                "flight_shaker_levels.json",
                include_str!("../../data/srs/flight_shaker_levels.json"),
            ),
            shaker_layouts: parse_dict("shaker_layouts.json", include_str!("../../data/srs/shaker_layouts.json")),
            ushake6_enhanced: parse_dict(
                "ushake6_enhanced_layout.json",
                include_str!("../../data/srs/ushake6_enhanced_layout.json"),
            ),
        })
    }

    /// Migration inputs for `platform`, with an optional `<docs>/profiles.json` from an imported folder.
    pub fn sources<'a>(&'a self, platform: Platform, legacy_layouts: &'a LegacyLayouts) -> MigrationSources<'a> {
        MigrationSources {
            platform,
            master_tunes: &self.master_tunes,
            expert_tunes: &self.expert_tunes,
            flight_shaker_levels: &self.flight_shaker_levels,
            shipped_layouts: &self.shaker_layouts,
            legacy_layouts,
            ushake6_enhanced: &self.ushake6_enhanced,
            registry: Registry::builtin(),
            effects: None,
        }
    }

    /// The stores of a brand-new installation before the migration chain runs,
    /// filled from the templates: `config.ini` = `template.ini`
    /// (which already holds `first_time_installation = yes`), `profile.db` = the template
    /// `profile.db`, and `metadata.db` = the same file, which has none of the metadata tables.
    pub fn fresh_install_state(&self) -> LegacyState {
        LegacyState {
            config: self.template_config.clone(),
            profiles: self.template_profiles.clone(),
            ..LegacyState::default()
        }
    }
}

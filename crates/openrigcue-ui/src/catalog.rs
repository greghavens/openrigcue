//! Read-only tables the pages need besides the service API: motion-model flags, telemetry channel labels, the
//! game feature table, factory tunes and the built-in speaker layouts.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use openrigcue_config::catalog::registry::Registry;
use openrigcue_config::catalog::FeatureTable;
use openrigcue_config::settings::Configuration;
use openrigcue_config::tunes::FactoryTunes;
use serde::{Deserialize, Serialize};

/// The pages' view of the motion model definitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFlags {
    pub name: String,
    pub manufacturer: String,
    pub motors: u32,
    /// Motion page Reaction Speed row (`show_reaction_speed`).
    pub show_reaction_speed: bool,
    /// Motion page Enhanced G-Forces row.
    pub enhanced_gforces: bool,
    /// Actuator assignment status and dialog (`axis_assignment`).
    pub axis_assignment: bool,
    /// Motion page refresh-rate row (`display_motion_rate`).
    pub display_motion_rate: bool,
    /// Hardware page port dropdowns (`manual_com_port`).
    pub manual_com_port: bool,
    /// The IntelliComp virtual sensor is offered (`enable_intellicomp`).
    pub enable_intellicomp: bool,
    /// Hardware page Reverse Traction Loss row (`allow_change_tl_dir`).
    pub allow_change_tl_dir: bool,
    /// The model defines `main_actuators`: the vertical-actuator rows and Max Horizontal are shown.
    pub main_actuators: bool,
    /// The model defines `belt_tensioner_motor`: the Belt Tensioner Factor row is shown.
    pub belt_tensioner_motor: bool,
    /// Actuator names by port (`port_mapping`).
    pub port_mapping: Vec<String>,
    /// The Axis Assignment dialog's rows (`assignment_options`), for the models with `axis_assignment`.
    #[serde(default)]
    pub assignment_options: Vec<String>,
}

/// Every motion model in registration order.
pub fn models() -> &'static [ModelFlags] {
    static MODELS: OnceLock<Vec<ModelFlags>> = OnceLock::new();
    MODELS.get_or_init(|| {
        openrigcue_model::motion_models::motion_models()
            .iter()
            .map(|model| {
                let mut fields = model.fields.clone();
                fields.insert("name".into(), toml::Value::String(model.key.clone()));
                let motors = fields.remove("nbr_motors").expect("canonical motion model has nbr_motors");
                fields.insert("motors".into(), motors);
                for field in ["main_actuators", "belt_tensioner_motor"] {
                    fields.insert(field.into(), toml::Value::Boolean(fields.contains_key(field)));
                }
                for field in ["enable_intellicomp", "allow_change_tl_dir"] {
                    fields.entry(field).or_insert(toml::Value::Boolean(false));
                }
                fields.entry("port_mapping").or_insert_with(|| toml::Value::Array(Vec::new()));
                toml::Value::Table(fields).try_into().expect("canonical motion model has valid UI fields")
            })
            .collect()
    })
}

/// A motion manufacturer as the Hardware page lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ManufacturerRow {
    pub name: String,
    pub default_refresh_rate: u32,
    /// Registered only on Windows.
    pub windows_only: bool,
}

/// The motion manufacturers in registration order.
pub fn manufacturers() -> Vec<ManufacturerRow> {
    Registry::builtin()
        .motion_manufacturers
        .iter()
        .map(|m| ManufacturerRow {
            name: m.name.clone(),
            default_refresh_rate: m.default_refresh_rate,
            windows_only: m.windows_only,
        })
        .collect()
}

/// A telemetry channel as the Telemetry page lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChannelRow {
    pub name: &'static str,
    pub label: &'static str,
    pub display: bool,
}

/// The telemetry channels in registry order.
pub fn channels() -> Vec<ChannelRow> {
    openrigcue_sources::derive::CHANNEL_SPECS
        .iter()
        .map(|spec| ChannelRow { name: spec.name, label: spec.label, display: spec.display })
        .collect()
}

/// The game features table: the columns, and for each game the columns it supports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FeatureDocument {
    pub columns: Vec<String>,
    pub games: BTreeMap<String, Vec<String>>,
}

/// The feature table as the pages read it: the columns, and each game's enabled columns.
pub fn features() -> FeatureDocument {
    let table = FeatureTable::builtin();
    FeatureDocument {
        columns: table.columns().to_vec(),
        games: table
            .game_ids()
            .filter_map(|id| table.game(id).map(|f| (id.to_owned(), f.enabled().to_vec())))
            .collect(),
    }
}

/// The factory tune of a game as JSON (vehicles included), for the Profile Manager reset and IntelliTune.
pub fn factory_tune(game: &str) -> Option<serde_json::Value> {
    FactoryTunes::builtin().get(game).map(|t| t.to_json())
}

/// Which speakers each built-in layout uses, keyed by layout name and
/// then by channel (`FL` … `RR`).
pub fn builtin_speaker_use() -> BTreeMap<String, BTreeMap<&'static str, bool>> {
    let layouts = &Configuration::first_run().shaker_layouts;
    Registry::builtin()
        .builtin_shaker_layouts
        .iter()
        .filter_map(|name| {
            let layout = layouts.get(name)?;
            let channels = [
                ("FL", layout.front_left.enable),
                ("FR", layout.front_right.enable),
                ("CE", layout.center.enable),
                ("SUB", layout.subwoofer.enable),
                ("RL", layout.rear_left.enable),
                ("RR", layout.rear_right.enable),
            ];
            Some((name.clone(), channels.into_iter().collect()))
        })
        .collect()
}

/// The motion model named `name`.
pub fn model(name: &str) -> Option<&'static ModelFlags> {
    models().iter().find(|m| m.name == name)
}

/// The configured motion platform model (`[MOTIONBOX] model`): an error when the stored value is not hashable or
/// names no model.
pub fn current_model(backend: &dyn crate::backend::Backend) -> Result<&'static ModelFlags, String> {
    use openrigcue_model::native_value::LegacyValue;
    let name = backend.native_setting("MOTIONBOX", "model").map_err(|error| error.detail)?;
    fn unhashable(value: &LegacyValue) -> Option<&'static str> {
        match value {
            LegacyValue::List(_) | LegacyValue::Dict(_) | LegacyValue::Set(_) => Some(value.type_name()),
            LegacyValue::Tuple(values) => values.iter().find_map(unhashable),
            _ => None,
        }
    }
    if let Some(kind) = unhashable(&name) {
        return Err(format!("TypeError: unhashable type: '{kind}'"));
    }
    if let LegacyValue::Str(name) = &name {
        if let Some(model) = model(name) {
            return Ok(model);
        }
    }
    Err(format!("KeyError: {}", openrigcue_model::python_repr::repr(&name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared definitions retain the configuration registry's model order and manufacturers.
    #[test]
    fn models_match_the_registry() {
        let ours: Vec<(&str, &str)> = models().iter().map(|m| (m.name.as_str(), m.manufacturer.as_str())).collect();
        let registry: Vec<(&str, &str)> =
            Registry::builtin().motion_models.iter().map(|m| (m.name.as_str(), m.manufacturer.as_str())).collect();
        assert_eq!(ours, registry);
    }

    /// motion_models flags of a few models: H6 has enhanced G-forces, T5PRO allows reversing traction loss and has
    /// main actuators, the 7-actuator Thanos belt rig has a belt tensioner motor, M2 has none of these.
    #[test]
    fn model_flags() {
        let find = |name: &str| models().iter().find(|m| m.name == name).unwrap();
        assert!(find("H6").enhanced_gforces && !find("H3").enhanced_gforces);
        assert!(find("T5PRO").allow_change_tl_dir && find("T5PRO").main_actuators && find("T5PRO").display_motion_rate);
        assert!(find("7 Actuators [2 Front + 2 Rear + TL + Surge + Belt]").belt_tensioner_motor);
        assert!(!find("8 Actuators [2 Front + 2 Rear + 2 TL + Surge + Belt]").belt_tensioner_motor);
        let m2 = find("M2");
        assert!(!m2.main_actuators && !m2.belt_tensioner_motor && m2.show_reaction_speed && m2.port_mapping.is_empty());
        assert!(!find("Yaw3").show_reaction_speed);
        assert_eq!(
            find("5 Actuators (2 Front + 2 Rear + Surge) CANBus").port_mapping,
            ["RL", "FL", "FR", "RR", "Surge"]
        );
    }

    /// The 79 registry channels with the Telemetry page's 70 displayed ones.
    #[test]
    fn channel_table() {
        let rows = channels();
        assert_eq!(rows.len(), 79);
        assert_eq!(rows.iter().filter(|r| r.display).count(), 70);
        assert_eq!(rows[0].label, "Pitch");
    }

    /// game_features.csv: the emulator row exists, "none" has no row (so the lookup fails in stand-by).
    #[test]
    fn feature_document() {
        let doc = features();
        assert!(doc.games.contains_key("emulator"));
        assert!(!doc.games.contains_key("none"));
        assert!(doc.columns.contains(&"sway".to_owned()) && !doc.columns.contains(&"surge".to_owned()));
    }

    /// The built-in layouts: "1 Speaker - Center" uses only CE.
    #[test]
    fn speaker_use() {
        let layouts = builtin_speaker_use();
        assert_eq!(layouts.len(), 9);
        let center = &layouts["1 Speaker - Center"];
        assert!(center["CE"] && !center["FL"] && !center["SUB"]);
        assert!(factory_tune("emulator").is_some() && factory_tune("nope").is_none());
    }
}

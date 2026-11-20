//! What the motion code reads from the configuration (sections of string values) and the game
//! configuration (the game's JSON tune): a value is converted at the point that reads it, and a
//! missing key or a malformed value is an error there.

use std::borrow::Cow;

use crate::error::CueError;
use crate::pyvalue::PyValue;

/// Why a configuration lookup found nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    /// The section is missing (a missing-key error naming the section).
    Section,
    /// The key is missing from its section (a missing-key error naming the key).
    Key,
}

/// The configuration: string values by section and key.
pub trait ConfigView: Send + Sync {
    /// Production global model lookup. Literal fixtures may bind one explicit Platform instead.
    fn current_model(&self) -> Option<Result<PyValue, CueError>> {
        None
    }
    fn current_test_value(&self, _index: usize) -> Option<PyValue> {
        None
    }
    fn current_game(&self) -> Option<String> {
        None
    }
    fn current_previous_game(&self) -> Option<String> {
        None
    }
    fn current_listener(&self) -> Option<crate::Listener> {
        None
    }
    fn current_game_kind(&self) -> Option<crate::GameKind> {
        None
    }
    /// The string value of `key` in `section`.
    fn value(&self, section: &str, key: &str) -> Result<Cow<'_, str>, Missing>;

    /// The actual value at the lookup. A settings tree may hold lists or in-memory numeric/bool values;
    /// the consumer, rather than this boundary, applies the integer, float or boolean conversion.
    fn read_value(&self, section: &str, key: &str) -> Result<PyValue, CueError> {
        match self.value(section, key) {
            Ok(value) => Ok(PyValue::Str(value.into_owned())),
            Err(Missing::Section) => Err(CueError::key(section)),
            Err(Missing::Key) => Err(CueError::key(key)),
        }
    }
}

/// The game configuration: the motion keys of the active game or vehicle tune, as dynamically typed values.
pub trait GameConfigView: Send + Sync {
    /// The value of `key`, `None` when the key is absent.
    fn item(&self, key: &str) -> Option<Cow<'_, PyValue>>;

    /// Preserves a type error when the active value itself cannot be indexed, as well as missing keys.
    fn read_item(&self, key: &str) -> Result<Cow<'_, PyValue>, CueError> {
        self.item(key).ok_or_else(|| CueError::key(key))
    }
}

/// A configuration value as a string.
pub(crate) fn config_str(config: &dyn ConfigView, section: &str, key: &str) -> Result<PyValue, CueError> {
    config.read_value(section, key)
}

/// A configuration value as an integer.
#[cfg(test)]
pub(crate) fn config_int(config: &dyn ConfigView, section: &str, key: &str) -> Result<i128, CueError> {
    config_integer(config, section, key)?.to_int()
}

/// A configuration value as an integer of any size, not narrowed before later arithmetic.
pub(crate) fn config_integer(config: &dyn ConfigView, section: &str, key: &str) -> Result<PyValue, CueError> {
    config_str(config, section, key)?.to_integer_value()
}

/// A configuration value as a float.
pub(crate) fn config_float(config: &dyn ConfigView, section: &str, key: &str) -> Result<f64, CueError> {
    config_str(config, section, key)?.to_float()
}

/// A configuration value as a boolean.
pub(crate) fn config_bool(config: &dyn ConfigView, section: &str, key: &str) -> Result<bool, CueError> {
    crate::pyvalue::str2bool(&config_str(config, section, key)?)
}

/// A game configuration value; a missing key is a missing-key error.
pub(crate) fn game_item<'a>(game: &'a dyn GameConfigView, key: &str) -> Result<Cow<'a, PyValue>, CueError> {
    game.read_item(key)
}

/// The entry `sub` of the game configuration value `key`.
pub(crate) fn game_item2(game: &dyn GameConfigView, key: &str, sub: &str) -> Result<PyValue, CueError> {
    game_item(game, key)?.item_owned(sub)
}

/// What one motion computation reads besides telemetry: the configuration, the game tune, the
/// configuration section of the model's manufacturer (`'DOF REALITY'` where the model has none)
/// and whether a game is running.
#[derive(Clone, Copy)]
pub(crate) struct Sources<'a> {
    pub config: &'a dyn ConfigView,
    pub game: &'a dyn GameConfigView,
    pub manufacturer: &'static str,
    pub game_running: &'a str,
}

impl Sources<'_> {
    pub fn model_field(&self, key: &str) -> Result<Option<PyValue>, CueError> {
        let Some(model) = self.config.current_model() else { return Ok(None) };
        let model = model?;
        let native = model.to_native().resolved();
        let text = native.python_string().and_then(|v| v.scalar_string());
        let Some(entry) = text.as_deref().and_then(crate::platform::motion_model) else {
            if matches!(
                native,
                openrigcue_model::native_value::LegacyValue::List(_)
                    | openrigcue_model::native_value::LegacyValue::Dict(_)
                    | openrigcue_model::native_value::LegacyValue::Set(_)
            ) {
                return Err(CueError::type_error(format!("unhashable type: '{}'", native.type_name())));
            }
            return Err(CueError::new(crate::error::Exception::KeyError, model.repr()));
        };
        fn convert(value: &toml::Value) -> PyValue {
            match value {
                toml::Value::String(v) => PyValue::Str(v.clone()),
                toml::Value::Integer(v) => PyValue::Int((*v).into()),
                toml::Value::Float(v) => PyValue::Float(*v),
                toml::Value::Boolean(v) => PyValue::Bool(*v),
                toml::Value::Array(v) => PyValue::List(v.iter().map(convert).collect()),
                toml::Value::Table(v) => PyValue::Dict(v.iter().map(|(k, v)| (k.clone(), convert(v))).collect()),
                toml::Value::Datetime(v) => PyValue::Str(v.to_string()),
            }
        }
        entry.fields.get(key).map(convert).map(Some).ok_or_else(|| CueError::key(key))
    }
    pub fn current_manufacturer(&self) -> &'static str {
        let Some(model) = self.config.current_model() else { return self.manufacturer };
        model
            .ok()
            .and_then(|v| v.to_native().python_string().and_then(|v| v.scalar_string()))
            .and_then(|model| crate::platform::motion_model(&model))
            .and_then(|model| model.fields.get("manufacturer"))
            .and_then(toml::Value::as_str)
            .unwrap_or("DOF REALITY")
    }
    pub fn with_current_manufacturer(&self) -> Self {
        Self { manufacturer: self.current_manufacturer(), ..*self }
    }
}

/// An INI-style configuration held as strings. Option names are matched case-insensitively
/// (they are stored in lower case), section names exactly; a key missing from a section is
/// looked up in `DEFAULT`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawConfig {
    sections: Vec<(String, Vec<(String, String)>)>,
}

impl RawConfig {
    pub fn new() -> RawConfig {
        RawConfig::default()
    }

    /// Sets `key` in `section` to `value`, creating the section.
    pub fn set(&mut self, section: &str, key: &str, value: &str) {
        let key = key.to_lowercase();
        let index = match self.sections.iter().position(|(s, _)| s == section) {
            Some(i) => i,
            None => {
                self.sections.push((section.to_owned(), Vec::new()));
                self.sections.len() - 1
            }
        };
        let entries = &mut self.sections[index].1;
        match entries.iter_mut().find(|(k, _)| *k == key) {
            Some(entry) => entry.1 = value.to_owned(),
            None => entries.push((key, value.to_owned())),
        }
    }

    /// Removes `key` from `section`.
    pub fn remove(&mut self, section: &str, key: &str) {
        let key = key.to_lowercase();
        if let Some((_, entries)) = self.sections.iter_mut().find(|(s, _)| s == section) {
            entries.retain(|(k, _)| *k != key);
        }
    }

    /// Removes `section` and its keys.
    pub fn remove_section(&mut self, section: &str) {
        self.sections.retain(|(s, _)| s != section);
    }

    fn lookup(&self, section: &str, key: &str) -> Option<&str> {
        self.sections.iter().find(|(s, _)| s == section)?.1.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

impl ConfigView for RawConfig {
    fn value(&self, section: &str, key: &str) -> Result<Cow<'_, str>, Missing> {
        let key = key.to_lowercase();
        let has_section = section == "DEFAULT" || self.sections.iter().any(|(s, _)| s == section);
        if !has_section {
            return Err(Missing::Section);
        }
        self.lookup(section, &key).or_else(|| self.lookup("DEFAULT", &key)).map(Cow::Borrowed).ok_or(Missing::Key)
    }
}

/// A game configuration held as the dictionary loaded from the game's JSON file.
#[derive(Debug, Clone, PartialEq)]
pub struct RawGameConfig(pub PyValue);

impl RawGameConfig {
    pub fn live(value: &openrigcue_model::native_value::LegacyValue) -> Self {
        Self(live_value(value))
    }
}
fn live_value(value: &openrigcue_model::native_value::LegacyValue) -> PyValue {
    use openrigcue_model::{native_object::NativeObject, native_value::LegacyValue};
    let value = if matches!(value, LegacyValue::Object(_)) {
        value.clone()
    } else {
        LegacyValue::Object(NativeObject::new(value.clone()))
    };
    PyValue::Native(value)
}

impl GameConfigView for RawGameConfig {
    fn item(&self, key: &str) -> Option<Cow<'_, PyValue>> {
        self.read_item(key).ok()
    }

    fn read_item(&self, key: &str) -> Result<Cow<'_, PyValue>, CueError> {
        if matches!(self.0, PyValue::Native(_)) {
            self.0.item_owned(key).map(Cow::Owned)
        } else {
            self.0.item(key).map(Cow::Borrowed)
        }
    }
}

/// The settings tree, with exact section/key spelling and value types. Unlike [`RawConfig`],
/// it does not lowercase keys or supply DEFAULT-section fallbacks.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeConfig(pub PyValue);

impl NativeConfig {
    pub fn live(value: &openrigcue_model::native_value::LegacyValue) -> Self {
        Self(live_value(value))
    }
}

impl ConfigView for NativeConfig {
    fn value(&self, section: &str, key: &str) -> Result<Cow<'_, str>, Missing> {
        let section = self.0.item_owned(section).map_err(|_| Missing::Section)?;
        let value = section.item_owned(key).map_err(|_| Missing::Key)?;
        value.to_native().python_string().and_then(|value| value.scalar_string()).map(Cow::Owned).ok_or(Missing::Key)
    }

    fn read_value(&self, section: &str, key: &str) -> Result<PyValue, CueError> {
        self.0.item_owned(section)?.item_owned(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configparser_lookups() {
        let mut c = RawConfig::new();
        c.set("MOTIONBOX", "Refresh_Rate", "30");
        c.set("DEFAULT", "fallback", "x");
        assert_eq!(config_int(&c, "MOTIONBOX", "refresh_rate"), Ok(30));
        assert_eq!(config_str(&c, "MOTIONBOX", "fallback"), Ok(PyValue::Str("x".into())));
        assert_eq!(config_str(&c, "DOF REALITY", "real_heave").unwrap_err().to_string(), "'DOF REALITY'");
        assert_eq!(config_str(&c, "MOTIONBOX", "model").unwrap_err().to_string(), "'model'");
        c.set("MOTIONBOX", "refresh_rate", "fast");
        assert_eq!(
            config_int(&c, "MOTIONBOX", "refresh_rate").unwrap_err().to_string(),
            "invalid literal for int() with base 10: 'fast'"
        );
    }

    #[test]
    fn native_config_preserves_types_case_and_missing_values() {
        let config = NativeConfig(PyValue::Dict(vec![
            (
                "MOTIONBOX".into(),
                PyValue::Dict(vec![
                    ("refresh_rate".into(), PyValue::Float(1.5)),
                    ("Refresh_Rate".into(), PyValue::Str("23".into())),
                    ("clip".into(), PyValue::List(vec![PyValue::Int(1)])),
                ]),
            ),
            ("DEFAULT".into(), PyValue::Dict(vec![("fallback".into(), PyValue::Str("12".into()))])),
        ]));
        assert_eq!(config_int(&config, "MOTIONBOX", "refresh_rate"), Ok(1));
        assert_eq!(config_int(&config, "MOTIONBOX", "Refresh_Rate"), Ok(23));
        assert_eq!(config_int(&config, "MOTIONBOX", "fallback").unwrap_err().to_string(), "'fallback'");
        assert_eq!(
            config_float(&config, "MOTIONBOX", "clip").unwrap_err().to_string(),
            "float() argument must be a string or a real number, not 'list'"
        );
        let game = RawGameConfig(PyValue::None);
        assert_eq!(
            game_item(&game, "motion_effects").unwrap_err().to_string(),
            "'NoneType' object is not subscriptable"
        );
    }
}

/// Raw copies of the typed settings and tune, for tests that change single values the way a config file
/// or a JSON tune could hold them.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::settings::MotionSettings;
    use crate::tune::MotionTune;

    pub fn native_config(settings: &MotionSettings, manufacturer: &str, model: &str) -> NativeConfig {
        use openrigcue_model::native_value::{Dict, LegacyValue};
        let raw = raw_config(settings, manufacturer);
        let mut dict = Dict::new();
        for (section, values) in raw.sections {
            let mut fields = Dict::new();
            for (key, value) in values {
                fields.insert(key, LegacyValue::Str(value));
            }
            if section == "MOTIONBOX" {
                fields.insert("model", LegacyValue::Str(model.to_owned()));
            }
            dict.insert(section, LegacyValue::Dict(fields));
        }
        NativeConfig::live(&LegacyValue::Dict(dict))
    }

    const GAME_KEYS: [&str; 17] = [
        "motion_effects",
        "motion_switch",
        "max_telemetry_vars",
        "motion_effect_smoothing_factor",
        "motion_smoothing_factor",
        "motion_boost_factor",
        "motion_max_power",
        "motion_max_horizontal",
        "min_traction_loss_activation",
        "min_traction_loss_front_activation",
        "motion_enhanced_gforces",
        "motion_belt_tensioner",
        "motion_main_actuators_sway",
        "motion_main_actuators_surge",
        "motion_main_actuators_traction_loss",
        "motion_main_actuators_traction_loss_front",
        "gear_smoothing_factor",
    ];

    const CONFIG_KEYS: [(&str, &[&str]); 3] = [
        (
            "MOTIONBOX",
            &[
                "refresh_rate",
                "soft_crash_factor",
                "soft_crash_enabled",
                "boost_multiplier",
                "sway_curving_factor",
                "surge_curving_factor",
                "auto_smoothing_low_speed",
                "auto_smoothing_speed",
                "auto_smoothing_slider",
                "gear_smoothing_min_delta",
                "overall_smoothing_map_min_slider",
                "overall_smoothing_map_max_slider",
            ],
        ),
        (
            "CUSTOM_FEATURES",
            &[
                "enable_legacy_gear_smoothing",
                "dams_front_tl_mode",
                "dams_tl_max_auto_smoothing",
                "dams_tl_auto_smoothing_max_speed_kmh",
            ],
        ),
        (
            "",
            &[
                "smoothing_threshold",
                "soft_crash_threshold",
                "boost_threshold",
                "soft_start_stop_factor",
                "soft_start_stop_timer",
                "real_heave",
            ],
        ),
    ];

    /// `tune` as a JSON game config.
    pub fn raw_game(tune: &MotionTune) -> RawGameConfig {
        RawGameConfig(PyValue::Dict(
            GAME_KEYS.iter().filter_map(|k| tune.item(k).map(|v| (k.to_string(), v.into_owned()))).collect(),
        ))
    }

    /// `settings` as a config file whose manufacturer section is `manufacturer`.
    pub fn raw_config(settings: &MotionSettings, manufacturer: &str) -> RawConfig {
        let mut raw = RawConfig::new();
        for (section, keys) in CONFIG_KEYS {
            for key in keys {
                if let Ok(v) = settings.value(if section.is_empty() { manufacturer } else { section }, key) {
                    raw.set(if section.is_empty() { manufacturer } else { section }, key, &v);
                }
            }
        }
        raw
    }

    /// Sets the game configuration value at the nested key `path`; `None` deletes the key.
    pub fn set_game(game: &mut RawGameConfig, path: &[&str], value: Option<PyValue>) {
        fn set(node: &mut PyValue, path: &[&str], value: Option<PyValue>) {
            let PyValue::Dict(entries) = node else { panic!("not a dict") };
            let position = entries.iter().position(|(k, _)| k == path[0]);
            if path.len() > 1 {
                set(&mut entries[position.expect("key")].1, &path[1..], value);
                return;
            }
            match (position, value) {
                (Some(i), Some(v)) => entries[i].1 = v,
                (None, Some(v)) => entries.push((path[0].to_owned(), v)),
                (Some(i), None) => {
                    entries.remove(i);
                }
                (None, None) => {}
            }
        }
        set(&mut game.0, path, value);
    }
}

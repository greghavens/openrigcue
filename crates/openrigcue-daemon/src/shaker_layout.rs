//! The loaded speaker layout snapshot and the individual dictionary operations on it.

use openrigcue_config::import::value::LegacyValue;
use openrigcue_model::python_repr::repr_str;
use serde_json::Value;

use crate::state::{LoadedLayout, ServiceEvent, ServiceState};
use crate::store::ConfigStore;

pub fn selected_name(store: &ConfigStore) -> Result<String, String> {
    let raw = store.settings().native_snapshot();
    let section = raw.get("SHAKEBOX").ok_or("KeyError: 'SHAKEBOX'")?;
    let value = section
        .item("profile")
        .map_err(|error| format!("{}: {error}", if section.as_dict().is_some() { "KeyError" } else { "TypeError" }))?;
    value.as_str().map(str::to_owned).ok_or_else(|| format!("TypeError: unsupported profile key {}", value.type_name()))
}

/// Assignment happens only after the selected database read and decode succeed.
pub fn load(state: &ServiceState) -> Result<(), String> {
    let (name, value) = {
        let store = state.config.read();
        let name = selected_name(&store)?;
        let value = store
            .metadata_row("shakers", &name)
            .map_err(|error| error.native_detail())?
            .ok_or_else(|| format!("KeyError: {}", repr_str(&name)))?;
        (name, value)
    };
    *state.shaker_layout.write() = LoadedLayout { name, value };
    *state.layout_edit.write() = None;
    state.config.write().update(|_| ());
    Ok(())
}

/// A UI assignment of SHAKEBOX.profile has no implicit reload or surround check.
pub fn assign_profile(state: &ServiceState, name: &str) -> Result<Value, String> {
    let mut store = state.config.write();
    let mut raw = store.settings().native_snapshot();
    raw.get_mut("SHAKEBOX")
        .ok_or("KeyError: 'SHAKEBOX'")?
        .assign_item("profile", LegacyValue::from(name))
        .map_err(|error| format!("TypeError: {error}"))?;
    store.update(|config| config.settings.edit_native(|target| *target = raw));
    let view = store.settings().ui_view().map_err(|error| format!("RuntimeError: {error}"));
    drop(store);
    state.announce(ServiceEvent::Settings);
    view
}

/// Copy only: the UI's surrounding callback owns membership, list and selection.
pub fn copy(state: &ServiceState, source: &str, name: &str) -> Result<(), String> {
    let mut store = state.config.write();
    let value = store
        .metadata_row("shakers", source)
        .map_err(|error| error.native_detail())?
        .ok_or_else(|| format!("KeyError: {}", repr_str(source)))?;
    store.set_shaker_entry(name, value).map_err(|error| error.native_detail())
}

pub fn remove(state: &ServiceState, name: &str) -> Result<(), String> {
    state
        .config
        .write()
        .delete_shaker_entry(name)
        .map_err(|error| error.native_detail())?
        .then_some(())
        .ok_or_else(|| format!("KeyError: {}", repr_str(name)))
}

/// Confirms the advanced shaker dialog: each completed assignment survives a later lookup/comparison failure.
pub fn confirm_levels(state: &ServiceState, speaker: Option<&str>, levels: &LegacyValue) -> Result<(), String> {
    fn channel<'a>(layout: &'a mut LegacyValue, speaker: Option<&str>) -> Result<&'a mut LegacyValue, String> {
        use openrigcue_config::import::value::Key;
        let key = speaker.map(Key::from).unwrap_or(Key::None);
        match layout {
            LegacyValue::Dict(channels) => channels
                .iter_mut()
                .find(|(candidate, _)| **candidate == key)
                .map(|(_, channel)| channel)
                .ok_or_else(|| format!("KeyError: {}", openrigcue_model::python_repr::repr(&key.to_value()))),
            value => {
                let detail = match speaker {
                    Some(speaker) => value.item(speaker).unwrap_err(),
                    None => match value {
                        LegacyValue::List(_) => "list indices must be integers or slices, not NoneType".into(),
                        LegacyValue::Tuple(_) => "tuple indices must be integers or slices, not NoneType".into(),
                        LegacyValue::Str(_) => "string indices must be integers, not 'NoneType'".into(),
                        LegacyValue::Bytes(_) => "byte indices must be integers or slices, not NoneType".into(),
                        value => format!("'{}' object is not subscriptable", value.type_name()),
                    },
                };
                Err(format!("TypeError: {detail}"))
            }
        }
    }
    fn field<'a>(value: &'a LegacyValue, key: &str) -> Result<&'a LegacyValue, String> {
        value
            .item(key)
            .map_err(|error| format!("{}: {error}", if value.as_dict().is_some() { "KeyError" } else { "TypeError" }))
    }
    fn maximum(first: &LegacyValue, second: &LegacyValue) -> Result<LegacyValue, String> {
        let comparison = openrigcue_model::python_compare::numeric_cmp(second, first).map_err(|error| {
            let (left, right) = error
                .strip_prefix("comparison between ")
                .and_then(|message| message.strip_suffix(" failed"))
                .and_then(|message| message.split_once(" and "))
                .unwrap_or((second.type_name(), first.type_name()));
            format!("TypeError: '>' not supported between instances of '{}' and '{}'", left, right)
        })?;
        Ok(if comparison == Some(std::cmp::Ordering::Greater) { second } else { first }.clone())
    }
    // Save takes the edit lock before reading the loaded layout.
    let mut pending = state.layout_edit.write();
    let mut loaded = state.shaker_layout.write();
    channel(&mut loaded.value, speaker)?
        .assign_item("sfx", levels.clone())
        .map_err(|error| format!("TypeError: {error}"))?;
    *pending = None;
    for (combined, first, second) in [("accel", "accel_lr", "accel_rr"), ("brake", "brake_lf", "brake_rf")] {
        let levels = field(channel(&mut loaded.value, speaker)?, "sfx")?;
        let value = maximum(field(levels, first)?, field(levels, second)?)?;
        let target = channel(&mut loaded.value, speaker)?;
        let levels = match target {
            LegacyValue::Dict(fields) => fields.get_mut("sfx").ok_or("KeyError: 'sfx'")?,
            value => return Err(format!("TypeError: {}", value.item("sfx").unwrap_err())),
        };
        levels.assign_item(combined, value).map_err(|error| format!("TypeError: {error}"))?;
    }
    Ok(())
}

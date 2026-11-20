//! Games that publish Windows shared memory, read through the bridge mirror.

pub mod answers;
pub mod assetto_corsa;
pub mod assetto_corsa_evo;
pub mod automobilista2;
pub mod block_reader;
mod estimated_wheel_slip;
pub mod falcon_bms;
pub mod held;
pub mod il2_cliffs;
pub mod iracing;
mod listener_channels;
mod liveness;
pub mod mapping;
mod packed;
pub mod pages;
mod pcars;
pub mod piboso;
pub mod project_cars;
pub mod project_cars2;
mod python_numeric;
pub mod raceroom;
pub mod rfactor1;
pub mod rfactor2;
pub mod simbin;
pub mod space_monkey;
pub mod trackmania;
pub mod trucksim;
pub mod warplanes;

/// Required APP pause setting value, read at each supported listener's getter.
pub type NativePauseReader =
    std::sync::Arc<dyn Fn() -> Option<openrigcue_model::native_value::LegacyValue> + Send + Sync>;

/// The current catalog game id (including the `none` sentinel).
pub type NativeGameReader = std::sync::Arc<dyn Fn() -> String + Send + Sync>;

/// Current window-title list published by the process/window scanner.
pub type NativeWindowsReader = std::sync::Arc<dyn Fn() -> Option<Vec<String>> + Send + Sync>;

pub(crate) fn native_pause(reader: &Option<NativePauseReader>, fallback: bool) -> Option<bool> {
    let Some(reader) = reader else { return Some(fallback) };
    let value = reader()?;
    if let openrigcue_model::native_value::LegacyValue::Bool(value) = value {
        return Some(value);
    }
    openrigcue_model::native_value::str2bool(&openrigcue_model::python_repr::str_native(&value).ok()?.scalar_string()?)
}

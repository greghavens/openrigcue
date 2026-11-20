//! One definition of the motion platform model table, shared by all consumers.

use std::sync::OnceLock;

use serde::Deserialize;

/// One model's key and complete insertion-ordered fields.
#[derive(Debug, Clone, PartialEq)]
pub struct MotionModel {
    pub key: String,
    pub fields: toml::Table,
}

#[derive(Deserialize)]
struct ModelsFile {
    model: Vec<toml::Table>,
    #[serde(default)]
    simracingtech_model: Vec<toml::Table>,
}

struct Definitions {
    registered: Vec<MotionModel>,
    simracingtech: Vec<MotionModel>,
}

fn definitions() -> &'static Definitions {
    static MODELS: OnceLock<Definitions> = OnceLock::new();
    MODELS.get_or_init(|| {
        let file: ModelsFile =
            toml::from_str(include_str!("../data/motion_models.toml")).expect("motion_models.toml is valid");
        let parse = |rows: Vec<toml::Table>| {
            rows.into_iter()
                .map(|mut fields| {
                    let key = match fields.remove("key") {
                        Some(toml::Value::String(key)) => key,
                        _ => panic!("motion_models.toml: an entry has no key"),
                    };
                    MotionModel { key, fields }
                })
                .collect()
        };
        Definitions { registered: parse(file.model), simracingtech: parse(file.simracingtech_model) }
    })
}

/// Models registered by default (`simracingtech_enabled` is false).
pub fn motion_models() -> &'static [MotionModel] {
    &definitions().registered
}

pub fn motion_model(key: &str) -> Option<&'static MotionModel> {
    motion_models().iter().find(|model| model.key == key)
}

/// Driver definitions also retain the two conditional models. Their
/// insertion point is after None; the bool identifies the required build flag.
pub fn all_motion_models() -> impl Iterator<Item = (&'static MotionModel, bool)> {
    let definitions = definitions();
    let (first, rest) = definitions.registered.split_first().expect("the None model is registered");
    std::iter::once((first, false))
        .chain(definitions.simracingtech.iter().map(|model| (model, true)))
        .chain(rest.iter().map(|model| (model, false)))
}

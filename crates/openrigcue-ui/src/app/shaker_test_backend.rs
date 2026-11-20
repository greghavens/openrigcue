//! In-memory shaker layout operations for UI fixtures; production uses SQLite.
use crate::backend::{Failure, Method};
use serde_json::{json, Value};

pub(crate) fn call(
    docs: &mut Value,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Option<Result<Value, Failure>> {
    if !matches!(
        path,
        "shakers/db"
            | "shakers/effects"
            | "shakers/effects/reload"
            | "shakers/assign-profile"
            | "shakers/rows/copy"
            | "shakers/rows/delete"
            | "shakers/surround-recheck"
    ) {
        return None;
    }
    if docs.get("shakers_db").is_none() {
        let table = if let Some(layouts) = docs.get("layouts").and_then(Value::as_array) {
            let mut table = serde_json::Map::new();
            for row in layouts {
                let name = row["name"].as_str().expect("fixture layout name");
                let mut channels = serde_json::Map::new();
                for (key, enabled) in row["speakers"].as_object().expect("fixture speakers") {
                    channels.insert(key.clone(), json!({"enable":enabled}));
                }
                table.insert(name.into(), Value::Object(channels));
            }
            Value::Object(table)
        } else {
            openrigcue_config::import::de::from_legacy(&openrigcue_model::native_value::LegacyValue::Dict(
                openrigcue_config::settings::Configuration::first_run().metadata_table("shakers"),
            ))
            .expect("fixture layouts")
        };
        docs["shakers_db"] = table;
    }
    let mut run = || -> Result<Value, Failure> {
        let failure = |detail| Failure { status: 500, detail };
        match (method, path) {
            (Method::Get, "shakers/db") => Ok(docs["shakers_db"].clone()),
            (Method::Get, "shakers/effects") => {
                if docs.get("loaded_shakers").is_none() {
                    let name = docs["settings"]["shakers"]["speaker_layout"].as_str().unwrap_or("");
                    docs["loaded_shakers"] = docs["shakers_db"].get(name).cloned().unwrap_or(Value::Null);
                }
                Ok(docs["loaded_shakers"].clone())
            }
            (Method::Post, "shakers/effects/reload") => {
                let name = docs["settings"]["shakers"]["speaker_layout"].as_str().unwrap_or("");
                let value =
                    docs["shakers_db"].get(name).cloned().ok_or_else(|| failure(format!("KeyError: {name:?}")))?;
                docs["loaded_shakers"] = value;
                Ok(Value::Null)
            }
            (Method::Post, "shakers/assign-profile") => {
                let section = docs["settings"]
                    .as_object_mut()
                    .and_then(|settings| settings.get_mut("shakers"))
                    .ok_or_else(|| failure("KeyError: 'SHAKEBOX'".into()))?;
                let fields = section
                    .as_object_mut()
                    .ok_or_else(|| failure("TypeError: invalid SHAKEBOX assignment parent".into()))?;
                fields.insert("speaker_layout".into(), body.expect("profile body")["name"].clone());
                Ok(docs["settings"].clone())
            }
            (Method::Post, "shakers/rows/copy") => {
                let body = body.expect("copy body");
                let source = body["source"].as_str().unwrap();
                let value =
                    docs["shakers_db"].get(source).cloned().ok_or_else(|| failure(format!("KeyError: {source:?}")))?;
                docs["shakers_db"].as_object_mut().unwrap().insert(body["name"].as_str().unwrap().into(), value);
                Ok(Value::Null)
            }
            (Method::Post, "shakers/rows/delete") => {
                let name = body.expect("delete body")["name"].as_str().unwrap();
                docs["shakers_db"]
                    .as_object_mut()
                    .unwrap()
                    .shift_remove(name)
                    .ok_or_else(|| failure(format!("KeyError: {name:?}")))?;
                Ok(Value::Null)
            }
            (Method::Post, "shakers/surround-recheck") => Ok(Value::Null),
            _ => Err(failure("unsupported fixture shaker operation".into())),
        }
    };
    Some(run())
}

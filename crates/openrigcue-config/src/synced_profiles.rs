//! Read-only access to the optional local SRS profile-sync library. Never reads credentials or the network.
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};

use openrigcue_model::native_value::LegacyValue;
use ring::digest::{digest, SHA256};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::profiles::{read_profile_content, read_profile_file, FileGame, FileVehicle};

const MAX_MANIFEST: u64 = 8 * 1024 * 1024;
const MAX_ARCHIVE: u64 = 16 * 1024 * 1024;
const MAX_UNPACKED: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Motion,
    Shakers,
    Belt,
    Rumble,
    Haptic,
}
impl Category {
    pub fn text(self) -> &'static str {
        match self {
            Self::Motion => "motion",
            Self::Shakers => "shakers",
            Self::Belt => "belt",
            Self::Rumble => "rumble",
            Self::Haptic => "haptic",
        }
    }
    pub fn page(self) -> &'static str {
        match self {
            Self::Motion => "Motion",
            Self::Shakers => "Shake",
            Self::Belt => "Belt",
            Self::Rumble => "SIM3D",
            Self::Haptic => "HapticR",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Vehicle {
    Game(u8),
    Named(String),
}
impl Vehicle {
    pub fn native(&self) -> LegacyValue {
        match self {
            Self::Game(_) => LegacyValue::Int(0),
            Self::Named(name) => name.clone().into(),
        }
    }
    pub fn label(&self) -> &str {
        match self {
            Self::Game(_) => "All vehicles",
            Self::Named(name) => name,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub game: String,
    pub page: Category,
    pub vehicle: Vehicle,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: u8,
    targets: Vec<Target>,
    records: BTreeMap<String, Record>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    target: Target,
    resource_id: String,
    remote_version: Value,
    alias: String,
    #[serde(default)]
    description: String,
    file: String,
    sha256: String,
    bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileState {
    Ready,
    Missing,
    Edited,
    Invalid(String),
}
impl FileState {
    pub fn label(&self) -> &str {
        match self {
            Self::Ready => "Ready",
            Self::Missing => "Missing file",
            Self::Edited => "Locally edited — not applied",
            Self::Invalid(message) => message,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Entry {
    pub id: String,
    pub target: Target,
    /// Exported tune origin; the cloud query target above remains the apply context.
    pub source_vehicle: Option<Vehicle>,
    pub author: String,
    /// The synced listing's equipment description; empty for records synced before it was stored.
    pub description: String,
    pub version: String,
    pub state: FileState,
}

pub fn default_root() -> Result<PathBuf, String> {
    let base = match std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
        Some(value) => {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err("XDG_DATA_HOME must be absolute".into());
            }
            path
        }
        None => PathBuf::from(std::env::var_os("HOME").ok_or("HOME is unset")?).join(".local/share"),
    };
    Ok(base.join("openrigcue/srs-profiles"))
}
fn bytes(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "Cannot read cached profile file".to_owned())?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit {
        return Err("Unsafe or oversized cached profile file".into());
    }
    let mut data = Vec::new();
    File::open(path)
        .map_err(|_| "Cannot open cached profile file")?
        .take(limit + 1)
        .read_to_end(&mut data)
        .map_err(|_| "Cannot read cached profile file")?;
    if data.len() as u64 > limit {
        return Err("Oversized cached profile file".into());
    }
    Ok(data)
}
fn hash(data: &[u8]) -> String {
    digest(&SHA256, data).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
fn validate_root(root: &Path) -> Result<(), String> {
    for path in [root.to_owned(), root.join("profiles")] {
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err("Profile library directories must not be symlinks".into());
        }
    }
    Ok(())
}
fn manifest(root: &Path) -> Result<Option<Manifest>, String> {
    validate_root(root)?;
    if !root.join("manifest.json").exists() {
        return Ok(None);
    }
    let data = bytes(&root.join("manifest.json"), MAX_MANIFEST)?;
    let manifest: Manifest = serde_json::from_slice(&data).map_err(|_| "Invalid downloaded-profile manifest")?;
    if manifest.format != 1 || manifest.targets.len() > 10_000 || manifest.records.len() > 100_000 {
        return Err("Unsupported or oversized downloaded-profile manifest".into());
    }
    for (id, record) in &manifest.records {
        let mut path = Path::new(&record.file).components();
        let safe = matches!(path.next(),Some(Component::Normal(part))if part=="profiles")
            && matches!(path.next(), Some(Component::Normal(_)))
            && path.next().is_none()
            && !record.file.contains('\\');
        let identity = hash(&serde_json::to_vec(&(&record.target, &record.resource_id)).map_err(|_| "Invalid target")?);
        if !safe
            || id != &identity
            || record.sha256.len() != 64
            || !record.sha256.bytes().all(|b| b.is_ascii_hexdigit())
            || !record.file.starts_with(&format!("profiles/{id}-{}", record.sha256))
            || !crate::catalog::Catalog::builtin().contains(&record.target.game)
            || !matches!(&record.target.vehicle, Vehicle::Game(0) | Vehicle::Named(_))
        {
            return Err("Invalid downloaded-profile manifest entry".into());
        }
    }
    Ok(Some(manifest))
}
fn state(root: &Path, record: &Record) -> FileState {
    if !root.join(&record.file).exists() {
        return FileState::Missing;
    }
    match bytes(&root.join(&record.file), MAX_ARCHIVE) {
        Ok(data) if data.len() as u64 == record.bytes && hash(&data) == record.sha256 => {
            match validate_archive(&data, &record.target) {
                Ok(_) => FileState::Ready,
                Err(error) => FileState::Invalid(error),
            }
        }
        Ok(_) => FileState::Edited,
        Err(error) => FileState::Invalid(error),
    }
}
pub fn list(root: &Path) -> Result<Vec<Entry>, String> {
    let Some(manifest) = manifest(root)? else { return Ok(Vec::new()) };
    let mut entries: Vec<_> = manifest
        .records
        .into_iter()
        .map(|(id, record)| {
            let state = state(root, &record);
            let source_vehicle = if state == FileState::Ready {
                bytes(&root.join(&record.file), MAX_ARCHIVE)
                    .ok()
                    .and_then(|data| read_profile_file(&data).ok())
                    .and_then(|profile| source_vehicle(profile.vehicle))
            } else {
                None
            };
            Entry {
                id,
                target: record.target.clone(),
                source_vehicle,
                author: record.alias.clone(),
                description: record.description.clone(),
                version: match &record.remote_version {
                    Value::String(s) => s.clone(),
                    value => value.to_string(),
                },
                state,
            }
        })
        .collect();
    entries.sort_by(|a, b| {
        (&a.target.game, a.target.page.text(), a.source_vehicle.as_ref().map(Vehicle::label), &a.author, &a.id).cmp(&(
            &b.target.game,
            b.target.page.text(),
            b.source_vehicle.as_ref().map(Vehicle::label),
            &b.author,
            &b.id,
        ))
    });
    Ok(entries)
}
pub fn load(root: &Path, id: &str) -> Result<(Entry, LegacyValue), String> {
    let manifest = manifest(root)?.ok_or("No downloaded profiles")?;
    let record = manifest.records.get(id).ok_or("Downloaded profile no longer exists in the manifest")?;
    let data = bytes(&root.join(&record.file), MAX_ARCHIVE)?;
    if hash(&data) != record.sha256 || data.len() as u64 != record.bytes {
        return Err(
            "The downloaded file was edited. Keep your copy and sync a separate original before applying.".into()
        );
    }
    let content = validate_archive(&data, &record.target)?;
    Ok((
        Entry {
            id: id.into(),
            target: record.target.clone(),
            source_vehicle: read_profile_file(&data).ok().and_then(|profile| source_vehicle(profile.vehicle)),
            author: record.alias.clone(),
            description: record.description.clone(),
            version: match &record.remote_version {
                Value::String(s) => s.clone(),
                v => v.to_string(),
            },
            state: FileState::Ready,
        },
        content,
    ))
}
fn source_vehicle(vehicle: FileVehicle) -> Option<Vehicle> {
    match vehicle {
        FileVehicle::Game => Some(Vehicle::Game(0)),
        FileVehicle::Named(name) => Some(Vehicle::Named(name)),
        _ => None,
    }
}

fn validate_archive(data: &[u8], target: &Target) -> Result<LegacyValue, String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(data)).map_err(|_| "Invalid profile ZIP")?;
    if zip.is_empty() || zip.len() > 64 {
        return Err("Invalid profile archive member count".into());
    }
    let mut total = 0;
    for index in 0..zip.len() {
        let mut member = zip.by_index(index).map_err(|_| "Invalid ZIP member")?;
        let name = member.name();
        if member.enclosed_name().is_none()
            || name.contains('\\')
            || name.contains(':')
            || member.is_dir()
            || Path::new(name)
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
            || member.unix_mode().is_some_and(|mode| mode & 0o170000 == 0o120000)
            || member.size() > MAX_UNPACKED - total
        {
            return Err("Unsafe or oversized profile archive".into());
        }
        let mut ignored = Vec::new();
        member.by_ref().take(MAX_UNPACKED - total + 1).read_to_end(&mut ignored).map_err(|_| "Invalid ZIP data")?;
        total += ignored.len() as u64;
        if total > MAX_UNPACKED {
            return Err("Oversized profile archive".into());
        }
    }
    let profile = read_profile_file(data).map_err(|_| "Missing profile metadata")?;
    if profile.game != FileGame::Id(target.game.clone()) || source_vehicle(profile.vehicle).is_none() {
        return Err("Profile metadata does not match its library entry".into());
    }
    read_profile_content(data).map_err(|_| "Invalid profile content".into())
}

#[cfg(test)]
mod tests;

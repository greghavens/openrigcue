//! Optional, user-initiated SRS cloud downloads. This never changes an active tune/database.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Args, ValueEnum};
use openrigcue_config::catalog::Catalog;
use openrigcue_config::profiles::{read_profile_file, FileGame, FileVehicle};
use ring::digest::{digest, SHA256};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const ENDPOINT: &str = "https://us-east4-srscloud.cloudfunctions.net/cloud_tune_sharing";
const MAX_KEY: u64 = 4096;
const MAX_LIST: u64 = 8 * 1024 * 1024;
const MAX_ARCHIVE: u64 = 16 * 1024 * 1024;
const MAX_UNPACKED: u64 = 32 * 1024 * 1024;
const MAX_MEMBERS: usize = 64;
const MAX_TARGETS: usize = 10_000;
const MAX_RECORDS: usize = 100_000;

#[derive(Args)]
pub struct KeyArgs {
    /// Private key file; default: $XDG_CONFIG_HOME/openrigcue/srs-sync/premium.key.
    #[arg(long)]
    key_file: Option<PathBuf>,
    /// Read one key from stdin (for a password manager); otherwise use a hidden terminal prompt.
    #[arg(long)]
    stdin: bool,
    /// Replace an existing private key file.
    #[arg(long)]
    replace: bool,
}

#[derive(Args)]
pub struct SyncArgs {
    /// List cached author/version/import paths without a key or network request.
    #[arg(long)]
    list: bool,
    /// Existing private key file, never the key itself.
    #[arg(long)]
    key_file: Option<PathBuf>,
    /// Separate download library; default: $XDG_DATA_HOME/openrigcue/srs-profiles.
    #[arg(long)]
    library_dir: Option<PathBuf>,
    /// Native game ID, repeatable. First use without targets selects all catalog game-level profiles.
    #[arg(long)]
    game: Vec<String>,
    /// Exact vehicle name for each --game, repeatable; game-level profiles are also included.
    #[arg(long, requires = "game")]
    vehicle: Vec<String>,
    /// Profile category, repeatable; defaults to all five categories for new targets.
    #[arg(long, value_enum)]
    page: Vec<Page>,
    /// JSON array of {game,page,vehicle}; vehicle is 0 or an exact name.
    #[arg(long)]
    targets: Option<PathBuf>,
    /// Add all catalog game-level targets even when a saved target list exists.
    #[arg(long)]
    all_games: bool,
    /// Also re-download unchanged versions to detect unversioned remote content changes.
    #[arg(long)]
    refresh: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Page {
    Motion,
    Shakers,
    Belt,
    Rumble,
    Haptic,
}

impl Page {
    const ALL: [Self; 5] = [Self::Motion, Self::Shakers, Self::Belt, Self::Rumble, Self::Haptic];
    fn text(self) -> &'static str {
        match self {
            Self::Motion => "motion",
            Self::Shakers => "shakers",
            Self::Belt => "belt",
            Self::Rumble => "rumble",
            Self::Haptic => "haptic",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
enum Vehicle {
    Game(u8),
    Named(String),
}

impl Vehicle {
    fn text(&self) -> String {
        match self {
            Self::Game(_) => "0".into(),
            Self::Named(name) => name.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    game: String,
    page: Page,
    vehicle: Vehicle,
}

impl Target {
    fn validate(&self) -> Result<()> {
        if !Catalog::builtin().contains(&self.game) {
            bail!("Target has an unknown native game ID");
        }
        match &self.vehicle {
            Vehicle::Game(0) => {}
            Vehicle::Named(name) if !name.is_empty() && name.len() <= 4096 && !name.chars().any(char::is_control) => {}
            _ => bail!("Target vehicle must be 0 or a nonempty vehicle name"),
        }
        Ok(())
    }

    fn identity(&self, resource: &str) -> String {
        // Structured serialization avoids delimiter collisions and retains the full resource ID.
        hash(&serde_json::to_vec(&(self, resource)).expect("serializable target"))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: u8,
    targets: Vec<Target>,
    records: BTreeMap<String, Record>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self { format: 1, targets: Vec::new(), records: BTreeMap::new() }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    target: Target,
    resource_id: String,
    remote_version: Value,
    alias: String,
    /// The picker's Description column for this category; absent in older manifests.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    description: String,
    file: String,
    sha256: String,
    bytes: u64,
}

/// Listing text shown in the picker: one bounded line without control characters.
fn listing_text(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?;
    Some(text.chars().filter(|character| !character.is_control()).take(256).collect())
}

/// The listing's `alias`, with `' [PRO]'` appended for a truthy `protuner`.
fn listing_alias(metadata: &Value) -> String {
    let mut alias = listing_text(metadata.get("alias")).unwrap_or_default();
    let protuner = match metadata.get("protuner") {
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_f64() != Some(0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
        Some(Value::Null) | None => false,
    };
    if protuner {
        alias.push_str(" [PRO]");
    }
    alias
}

/// The equipment description of one listing row by page.
/// An incomplete row is kept without a description.
fn listing_description(page: Page, metadata: &Value) -> String {
    let field = |key: &str| listing_text(metadata.get(key));
    let bracketed = |prefix: Option<String>, model: &str| Some(format!("{} [{}]", prefix?, field(model)?));
    match page {
        Page::Motion => bracketed(field("motion_manufacturer"), "motion_model"),
        Page::Shakers => field("shakers_profile"),
        Page::Belt => bracketed(field("belt_manufacturer"), "belt_model"),
        Page::Rumble => bracketed(Some("SIM3D".into()), "rumble_model"),
        Page::Haptic => bracketed(Some("HAPTIC".into()), "haptic_model"),
    }
    .unwrap_or_default()
}

fn hash(bytes: &[u8]) -> String {
    digest(&SHA256, bytes).as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

fn xdg_path(variable: &str, fallback: &str) -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(variable).filter(|value| !value.is_empty()) {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            bail!("{variable} must be an absolute directory");
        }
        return Ok(path);
    }
    let home =
        std::env::var_os("HOME").filter(|value| !value.is_empty()).context("HOME is unset; pass explicit paths")?;
    Ok(PathBuf::from(home).join(fallback))
}

fn key_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    explicit.map_or_else(|| Ok(xdg_path("XDG_CONFIG_HOME", ".config")?.join("openrigcue/srs-sync/premium.key")), Ok)
}

fn library_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    explicit.map_or_else(|| Ok(xdg_path("XDG_DATA_HOME", ".local/share")?.join("openrigcue/srs-profiles")), Ok)
}

fn bounded_read(mut input: impl Read, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    input.by_ref().take(limit + 1).read_to_end(&mut bytes).context("Could not read bounded input")?;
    if bytes.len() as u64 > limit {
        bail!("Input exceeds the permitted size");
    }
    Ok(bytes)
}

fn validate_key(bytes: &[u8]) -> Result<String> {
    let key = std::str::from_utf8(bytes).context("Key is not UTF-8")?.trim();
    if key.is_empty()
        || key.len() as u64 > MAX_KEY
        || key.chars().any(char::is_whitespace)
        || key.chars().any(char::is_control)
    {
        bail!("Key must be one nonempty line without whitespace");
    }
    Ok(key.to_owned())
}

#[cfg(unix)]
fn private_file(path: &Path) -> Result<File> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = fs::symlink_metadata(path).context("Cannot inspect private key file; run srs-key first")?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.permissions().mode() & 0o077 != 0 {
        bail!("Key file must be a regular private file (chmod 600), not a symlink");
    }
    File::open(path).context("Cannot open private key file")
}

#[cfg(not(unix))]
fn private_file(path: &Path) -> Result<File> {
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        bail!("Key file must not be a symlink");
    }
    File::open(path).context("Cannot open private key file")
}

fn read_key(path: &Path) -> Result<String> {
    validate_key(&bounded_read(private_file(path)?, MAX_KEY)?)
}

/// A scoped terminal echo change, restored on normal return and errors.
#[cfg(unix)]
struct EchoGuard(String);
#[cfg(unix)]
impl Drop for EchoGuard {
    fn drop(&mut self) {
        let _ = std::process::Command::new("stty").arg(&self.0).stdin(std::process::Stdio::inherit()).status();
    }
}

fn prompt_key() -> Result<String> {
    if !io::stdin().is_terminal() {
        bail!("Use --stdin with a password manager, or run srs-key in a terminal");
    }
    #[cfg(unix)]
    {
        let state = std::process::Command::new("stty")
            .arg("-g")
            .stdin(std::process::Stdio::inherit())
            .output()
            .context("Cannot disable terminal echo; use --stdin")?;
        if !state.status.success() {
            bail!("Cannot read terminal settings; use --stdin");
        }
        let state = String::from_utf8(state.stdout).context("Cannot read terminal settings")?;
        if !std::process::Command::new("stty").arg("-echo").stdin(std::process::Stdio::inherit()).status()?.success() {
            bail!("Cannot disable terminal echo; use --stdin");
        }
        let guard = EchoGuard(state.trim().into());
        let restore = guard.0.clone();
        ctrlc::set_handler(move || {
            let _ = std::process::Command::new("stty").arg(&restore).stdin(std::process::Stdio::inherit()).status();
            std::process::exit(130);
        })
        .context("Cannot install terminal interrupt recovery")?;
        eprint!("SRS Premium key (hidden): ");
        io::stderr().flush()?;
        let mut line = Vec::new();
        // A bounded line read without waiting for terminal EOF.
        for byte in io::stdin().lock().bytes().take((MAX_KEY + 1) as usize) {
            let byte = byte?;
            if byte == b'\n' {
                break;
            }
            line.push(byte);
        }
        drop(guard);
        eprintln!();
        validate_key(&line)
    }
    #[cfg(not(unix))]
    bail!("Hidden prompt is unavailable on this platform; use --stdin with a password manager")
}

pub fn setup_key(args: KeyArgs) -> Result<()> {
    let path = key_path(args.key_file)?;
    let key = if args.stdin { validate_key(&bounded_read(io::stdin().lock(), MAX_KEY)?)? } else { prompt_key()? };
    let parent = path.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(parent).context("Cannot create key directory")?;
    #[cfg(unix)]
    if parent.file_name().is_some_and(|name| name == "srs-sync") {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    if path.exists() && !args.replace {
        bail!("Key already exists; use --replace to replace it");
    }
    if fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!("Refusing to replace a symlink key file");
    }
    atomic_write(&path, key.as_bytes(), !args.replace)?;
    println!("Private key saved. Run openrigcue srs-sync to sync your separate profile library.");
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8], no_replace: bool) -> Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(parent).context("Cannot stage local file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file().set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes).context("Cannot write staged local file")?;
    file.as_file().sync_all().context("Cannot sync staged local file")?;
    if no_replace {
        file.persist_noclobber(path)
            .map_err(|_| anyhow::anyhow!("Local file already exists or could not be installed"))?;
    } else {
        file.persist(path).map_err(|_| anyhow::anyhow!("Cannot atomically install local file"))?;
    }
    #[cfg(unix)]
    File::open(parent)?.sync_all().context("Cannot sync local directory")?;
    Ok(())
}

trait Transport {
    fn post(&mut self, fields: &[(&str, &str)], limit: u64) -> Result<Vec<u8>>;
}

#[derive(Debug)]
struct AuthRejected;
impl std::fmt::Display for AuthRejected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SRS rejected the Premium key (HTTP 401/403)")
    }
}
impl std::error::Error for AuthRejected {}

#[derive(Debug)]
struct RequestFailure {
    category: &'static str,
    status: Option<u16>,
}
impl std::fmt::Display for RequestFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(status) => write!(formatter, "SRS request failed: {} (HTTP {status})", self.category),
            None => write!(formatter, "SRS request failed: {}", self.category),
        }
    }
}
impl std::error::Error for RequestFailure {}

struct HttpsTransport(reqwest::blocking::Client);
impl HttpsTransport {
    fn new() -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .user_agent("OpenRigCue profile sync")
            .build()
            .map_err(|_| anyhow::anyhow!("Cannot create secure SRS client"))?;
        Ok(Self(client))
    }
}
impl Transport for HttpsTransport {
    fn post(&mut self, fields: &[(&str, &str)], limit: u64) -> Result<Vec<u8>> {
        let response = self.0.post(ENDPOINT).form(fields).send().map_err(|error| RequestFailure {
            category: if error.is_timeout() { "timeout" } else { "network_or_tls" },
            status: None,
        })?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            // Never reflect response bodies, request objects, or credential-bearing errors.
            return Err(RequestFailure {
                category: if matches!(status, 401 | 403) { "authentication" } else { "http_status" },
                status: Some(status),
            }
            .into());
        }
        if response.content_length().is_some_and(|size| size > limit) {
            return Err(RequestFailure { category: "response_too_large", status: Some(status) }.into());
        }
        bounded_read(response, limit)
            .map_err(|_| RequestFailure { category: "response_read_or_size", status: Some(status) }.into())
    }
}

/// One private, append-only JSONL file per run. Never serializes request/error objects or response bodies.
struct RunLog {
    file: File,
    path: PathBuf,
    key: String,
}
impl RunLog {
    fn new(root: &Path, key: &str) -> Result<Self> {
        let directory = root.join("logs");
        if fs::symlink_metadata(&directory)
            .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_dir())
        {
            bail!("Sync log directory must be a real directory");
        }
        fs::create_dir_all(&directory).context("Cannot create sync log directory")?;
        let temporary = tempfile::Builder::new().prefix("sync-").suffix(".jsonl").tempfile_in(directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary.as_file().set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        let (file, path) = temporary.keep().context("Cannot retain sync log")?;
        Ok(Self { file, path, key: key.into() })
    }

    fn event(&mut self, mut event: Value) -> Result<()> {
        fn redact(value: &mut Value, key: &str) {
            match value {
                Value::String(text) if !key.is_empty() => *text = text.replace(key, "[redacted]"),
                Value::Array(values) => values.iter_mut().for_each(|value| redact(value, key)),
                Value::Object(values) => values.values_mut().for_each(|value| redact(value, key)),
                _ => {}
            }
        }
        event["unix_ms"] = serde_json::json!(std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis());
        redact(&mut event, &self.key);
        let mut line = serde_json::to_vec(&event)?;
        line.push(b'\n');
        self.file.write_all(&line).context("Cannot write sync log")?;
        self.file.flush().context("Cannot flush sync log")?;
        Ok(())
    }
}

struct TargetTrace {
    stage: &'static str,
    resource_id: Option<String>,
    version: Option<String>,
}
impl TargetTrace {
    fn new() -> Self {
        Self { stage: "list_request", resource_id: None, version: None }
    }
    fn failure(&self, error: &anyhow::Error) -> (&'static str, Option<u16>) {
        if let Some(error) = error.downcast_ref::<RequestFailure>() {
            (error.category, error.status)
        } else if error.downcast_ref::<AuthRejected>().is_some() {
            ("authentication", None)
        } else {
            (
                match self.stage {
                    "list_request" | "download_request" => "transport",
                    "list_decode" => "invalid_listing",
                    "resource_metadata" => "invalid_resource_metadata",
                    "archive_validation" => "invalid_archive_or_metadata",
                    _ => "local_library_io_or_limit",
                },
                None,
            )
        }
    }
}

struct LibraryLock(PathBuf);
impl Drop for LibraryLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn lock_library(root: &Path) -> Result<LibraryLock> {
    let path = root.join(".sync.lock");
    let mut file = OpenOptions::new().write(true).create_new(true).open(&path).context(
        "Library is locked; another sync may be running. Remove .sync.lock only after that process has ended",
    )?;
    writeln!(file, "{}", std::process::id())?;
    Ok(LibraryLock(path))
}

fn read_manifest(root: &Path) -> Result<Manifest> {
    let path = root.join("manifest.json");
    if !path.exists() {
        return Ok(Manifest::default());
    }
    if fs::symlink_metadata(&path)?.file_type().is_symlink() {
        bail!("Manifest must not be a symlink");
    }
    let bytes = bounded_read(File::open(path)?, MAX_LIST)?;
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|_| {
        anyhow::anyhow!("Invalid local manifest; keep it for recovery and choose another --library-dir")
    })?;
    if manifest.format != 1 || manifest.targets.len() > MAX_TARGETS || manifest.records.len() > MAX_RECORDS {
        bail!("Unsupported or oversized local manifest");
    }
    for target in &manifest.targets {
        target.validate()?;
    }
    for (identity, record) in &manifest.records {
        record.target.validate()?;
        if identity != &record.target.identity(&record.resource_id)
            || !safe_local_file(&record.file)
            || record.sha256.len() != 64
            || !record.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !record.file.starts_with(&format!("profiles/{identity}-{}", record.sha256))
        {
            bail!("Invalid local manifest record");
        }
    }
    Ok(manifest)
}

fn safe_local_file(name: &str) -> bool {
    let mut parts = Path::new(name).components();
    matches!(parts.next(), Some(Component::Normal(part)) if part == "profiles")
        && matches!(parts.next(), Some(Component::Normal(_)))
        && parts.next().is_none()
        && !name.contains('\\')
}

fn save_manifest(root: &Path, manifest: &Manifest) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest)?;
    if bytes.len() as u64 > MAX_LIST {
        bail!("Local manifest has reached its size limit");
    }
    atomic_write(&root.join("manifest.json"), &bytes, false)
}

fn choose_targets(args: &SyncArgs, saved: &[Target]) -> Result<Vec<Target>> {
    let explicit = !args.game.is_empty()
        || !args.vehicle.is_empty()
        || !args.page.is_empty()
        || args.targets.is_some()
        || args.all_games;
    let mut targets = saved.to_vec();
    if let Some(path) = &args.targets {
        let bytes = bounded_read(File::open(path).context("Cannot open targets JSON")?, MAX_LIST)?;
        let imported: Vec<Target> = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("Invalid targets JSON; expected an array of game/page/vehicle records"))?;
        for target in imported {
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
    }
    if !args.game.is_empty()
        || args.all_games
        || (!explicit && saved.is_empty())
        || (!args.page.is_empty() && args.targets.is_none())
    {
        let games: Vec<&str> = if args.game.is_empty() {
            Catalog::builtin().games().iter().map(|game| game.id.as_str()).collect()
        } else {
            args.game.iter().map(String::as_str).collect()
        };
        let pages = if args.page.is_empty() { &Page::ALL[..] } else { &args.page };
        for game in games {
            for page in pages {
                for vehicle in std::iter::once(Vehicle::Game(0)).chain(args.vehicle.iter().cloned().map(Vehicle::Named))
                {
                    let target = Target { game: game.into(), page: *page, vehicle };
                    if !targets.contains(&target) {
                        targets.push(target);
                    }
                }
            }
        }
    }
    if targets.len() > MAX_TARGETS {
        bail!("Too many sync targets");
    }
    for target in &targets {
        target.validate()?;
    }
    Ok(targets)
}

fn version_text(value: &Value) -> Result<String> {
    match value {
        Value::String(text) if !text.is_empty() && text.len() <= 1024 && !text.chars().any(char::is_control) => {
            Ok(text.clone())
        }
        Value::Number(number) => Ok(number.to_string()),
        _ => bail!("SRS listing contains an invalid profile version"),
    }
}

fn validate_archive(bytes: &[u8], target: &Target) -> Result<()> {
    if bytes.len() as u64 > MAX_ARCHIVE {
        bail!("Profile archive exceeds size limit");
    }
    let mut zip = zip::ZipArchive::new(io::Cursor::new(bytes))
        .map_err(|_| anyhow::anyhow!("SRS download is not a profile ZIP"))?;
    if zip.is_empty() || zip.len() > MAX_MEMBERS {
        bail!("Profile archive has an invalid member count");
    }
    let mut total = 0_u64;
    for index in 0..zip.len() {
        let mut member = zip.by_index(index).map_err(|_| anyhow::anyhow!("Invalid profile ZIP member"))?;
        let name = member.name();
        if member.enclosed_name().is_none()
            || name.contains('\\')
            || name.contains(':')
            || member.is_dir()
            || Path::new(name)
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
            || member.unix_mode().is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            bail!("Profile ZIP contains an unsafe member path or type");
        }
        if member.size() > MAX_UNPACKED.saturating_sub(total) {
            bail!("Profile ZIP expands beyond size limit");
        }
        let content = bounded_read(&mut member, MAX_UNPACKED - total)
            .map_err(|_| anyhow::anyhow!("Profile ZIP member is invalid or too large"))?;
        total += content.len() as u64;
    }
    let profile = read_profile_file(bytes)
        .map_err(|_| anyhow::anyhow!("Profile is missing required game/vehicle/version metadata"))?;
    if profile.game != FileGame::Id(target.game.clone()) {
        bail!("Downloaded profile is for a different game");
    }
    // The cloud query identifies the selection context, not necessarily the vehicle
    // the tune was exported for. Category values apply without
    // comparing the exported vehicle with that query.
    if !matches!(profile.vehicle, FileVehicle::Game | FileVehicle::Named(_)) {
        bail!("Downloaded profile has invalid vehicle metadata");
    }
    Ok(())
}

fn local_matches(root: &Path, record: &Record) -> Result<bool> {
    let path = root.join(&record.file);
    if !path.exists() {
        return Ok(false);
    }
    let metadata = fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("Refusing a non-regular local profile file");
    }
    if metadata.len() != record.bytes {
        return Ok(false);
    }
    Ok(hash(&bounded_read(File::open(path)?, MAX_ARCHIVE)?) == record.sha256)
}

fn store_archive(root: &Path, identity: &str, bytes: &[u8]) -> Result<(String, String)> {
    let sha = hash(bytes);
    for suffix in 0..10_000 {
        let name = if suffix == 0 {
            format!("profiles/{identity}-{sha}.srs")
        } else {
            format!("profiles/{identity}-{sha}-{suffix}.srs")
        };
        let path = root.join(&name);
        if path.exists() {
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.len() <= MAX_ARCHIVE
                && hash(&bounded_read(File::open(&path)?, MAX_ARCHIVE)?) == sha
            {
                return Ok((name, sha));
            }
            continue; // A user-edited file is retained, even at the content-addressed path.
        }
        atomic_write(&path, bytes, true)?;
        return Ok((name, sha));
    }
    bail!("Too many local filename collisions")
}

#[derive(Default, Debug, PartialEq, Eq)]
struct Counts {
    downloaded: usize,
    unchanged: usize,
    preserved: usize,
    failures: usize,
}

fn sync(
    transport: &mut impl Transport,
    root: &Path,
    key: &str,
    manifest: &mut Manifest,
    refresh: bool,
    out: &mut impl Write,
) -> Result<Counts> {
    let mut log = RunLog::new(root, key)?;
    writeln!(out, "Sync log: {}", log.path.display())?;
    log.event(serde_json::json!({"event":"run_started", "targets":manifest.targets.len(), "refresh":refresh}))?;
    let mut counts = Counts::default();
    let mut consecutive_failures = 0;
    if let Err(error) = save_manifest(root, manifest) {
        log.event(serde_json::json!({"event":"run_failed", "category":"local_manifest_write"}))?;
        return Err(error);
    } // New explicit targets survive a failed connection.
    for (index, target) in manifest.targets.clone().into_iter().enumerate() {
        writeln!(
            out,
            "[{}/{}] {} / {} / {}",
            index + 1,
            manifest.targets.len(),
            target.game,
            target.page.text(),
            target.vehicle.text()
        )?;
        log.event(serde_json::json!({"event":"target_started", "target":target}))?;
        let mut trace = TargetTrace::new();
        let result =
            sync_target(transport, root, key, manifest, &target, refresh, (&mut counts, out, &mut log, &mut trace));
        if let Err(error) = result {
            counts.failures += 1;
            consecutive_failures += 1;
            let (category, status) = trace.failure(&error);
            log.event(serde_json::json!({"event":"target_failed", "target":target, "resource_id":trace.resource_id,
                "version":trace.version, "stage":trace.stage, "category":category, "http_status":status}))?;
            writeln!(out, "  Failed: {category} at {}; HTTP {status:?}. See sync log.", trace.stage)?;
            if category == "authentication" || consecutive_failures >= 3 {
                writeln!(out, "Stopped after authentication rejection or three consecutive failed targets. Saved targets and completed downloads are retained.")?;
                log.event(serde_json::json!({"event":"run_stopped", "category":category}))?;
                break;
            }
        } else {
            consecutive_failures = 0;
            log.event(serde_json::json!({"event":"target_finished", "target":target}))?;
        }
    }
    log.event(serde_json::json!({"event":"run_finished", "downloaded":counts.downloaded,
        "unchanged":counts.unchanged, "preserved":counts.preserved, "failures":counts.failures}))?;
    log.file.sync_all().context("Cannot sync completed run log")?;
    Ok(counts)
}

fn sync_target(
    transport: &mut impl Transport,
    root: &Path,
    key: &str,
    manifest: &mut Manifest,
    target: &Target,
    refresh: bool,
    progress: (&mut Counts, &mut impl Write, &mut RunLog, &mut TargetTrace),
) -> Result<()> {
    let (counts, out, log, trace) = progress;
    let vehicle = target.vehicle.text();
    let list = transport.post(
        &[
            ("action", "list_resources"),
            ("key", key),
            ("game", &target.game),
            ("page", target.page.text()),
            ("vehicle", &vehicle),
        ],
        MAX_LIST,
    )?;
    trace.stage = "list_decode";
    let list: Value = serde_json::from_slice(&list)
        .map_err(|_| anyhow::anyhow!("SRS returned an invalid listing (check your Premium key)"))?;
    let rows = list.as_object().context("SRS did not return a profile listing (check your Premium key)")?;
    if rows.len() > MAX_RECORDS {
        bail!("SRS listing has too many profiles");
    }
    for (resource_id, metadata) in rows {
        trace.stage = "resource_metadata";
        trace.resource_id = Some(resource_id.clone());
        trace.version = None;
        if resource_id.is_empty() || resource_id.len() > 4096 || resource_id.chars().any(char::is_control) {
            bail!("SRS returned an invalid resource ID");
        }
        let version = metadata.get("current_version").context("SRS listing is missing a profile version")?;
        let version_string = version_text(version)?;
        trace.version = Some(version_string.clone());
        log.event(serde_json::json!({"event":"resource_started", "target":target,
            "resource_id":resource_id, "version":version_string}))?;
        let identity = target.identity(resource_id);
        let alias = listing_alias(metadata);
        let description = listing_description(target.page, metadata);
        trace.stage = "local_verification";
        // The listing is the only source of the picker's alias and description: keep an unchanged
        // download's labels current, and fill them for records made before they were stored.
        if let Some(record) = manifest.records.get_mut(&identity) {
            if !refresh
                && record.remote_version == *version
                && (record.alias != alias || record.description != description)
            {
                record.alias = alias.clone();
                record.description = description.clone();
                save_manifest(root, manifest)?;
            }
        }
        if let Some(record) = manifest.records.get(&identity) {
            if !refresh && record.remote_version == *version {
                if local_matches(root, record)? {
                    counts.unchanged += 1;
                    log.event(serde_json::json!({"event":"unchanged", "target":target,
                        "resource_id":resource_id, "version":version_string}))?;
                    continue;
                }
                if root.join(&record.file).exists() {
                    writeln!(out, "  Preserved locally edited file: {}", record.file)?;
                    counts.preserved += 1;
                    log.event(serde_json::json!({"event":"preserved_local_edit", "target":target,
                        "resource_id":resource_id, "version":version_string, "file":record.file}))?;
                    continue;
                }
            }
        }
        trace.stage = "download_request";
        let archive = transport.post(
            &[
                ("action", "download_resource"),
                ("key", key),
                ("game", &target.game),
                ("vehicle", &vehicle),
                ("resource_id", resource_id),
                ("version", &version_string),
            ],
            MAX_ARCHIVE,
        )?;
        trace.stage = "archive_validation";
        validate_archive(&archive, target)?;
        trace.stage = "local_store";
        if manifest.records.len() >= MAX_RECORDS && !manifest.records.contains_key(&identity) {
            bail!("Local library has too many profile records");
        }
        let (file, sha256) = store_archive(root, &identity, &archive)?;
        writeln!(out, "  Downloaded: {file}")?;
        manifest.records.insert(
            identity,
            Record {
                target: target.clone(),
                resource_id: resource_id.clone(),
                remote_version: version.clone(),
                alias,
                description,
                file,
                sha256,
                bytes: archive.len() as u64,
            },
        );
        save_manifest(root, manifest)?;
        counts.downloaded += 1;
        log.event(serde_json::json!({"event":"downloaded", "target":target,
            "resource_id":resource_id, "version":version_string}))?;
    }
    Ok(())
}

pub fn run(args: SyncArgs) -> Result<()> {
    let root = library_path(args.library_dir.clone())?;
    if args.list {
        return list_cached(&root, &mut io::stdout().lock());
    }
    let key = read_key(&key_path(args.key_file.clone())?)?;
    for path in [&root, &root.join("profiles")] {
        if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            bail!("Library and profiles directories must not be symlinks");
        }
    }
    fs::create_dir_all(root.join("profiles")).context("Cannot create separate profile library")?;
    let _lock = lock_library(&root)?;
    let mut manifest = read_manifest(&root)?;
    manifest.targets = choose_targets(&args, &manifest.targets)?;
    let mut transport = HttpsTransport::new()?;
    let counts = sync(&mut transport, &root, &key, &mut manifest, args.refresh, &mut io::stdout().lock())?;
    println!(
        "Sync complete: {} downloaded, {} unchanged, {} locally edited files preserved, {} target failures.",
        counts.downloaded, counts.unchanged, counts.preserved, counts.failures
    );
    println!(
        "Library: {}\nNo active tune was changed. In the app, open Downloaded Profiles on a tuning page or in the Profile Manager to apply one category, or import a whole .srs file with openrigcue run --profile-path FILE.",
        root.display()
    );
    if counts.failures != 0 {
        bail!("Some targets failed; successful downloads are saved. Rerun to retry.");
    }
    Ok(())
}

fn list_cached(root: &Path, out: &mut impl Write) -> Result<()> {
    let manifest = read_manifest(root)?;
    if manifest.records.is_empty() {
        writeln!(out, "No cached profiles in {}", root.display())?;
    }
    for record in manifest.records.values() {
        let status = if local_matches(root, record)? { "cached" } else { "missing or locally edited" };
        let source_vehicle = if status == "cached" {
            let data = bounded_read(File::open(root.join(&record.file))?, MAX_ARCHIVE)?;
            validate_archive(&data, &record.target)?;
            match read_profile_file(&data)?.vehicle {
                FileVehicle::Game => "All vehicles".into(),
                FileVehicle::Named(name) => name,
                _ => unreachable!("validated vehicle metadata"),
            }
        } else {
            "Unavailable".into()
        };
        let equipment = if record.description.is_empty() {
            String::new()
        } else {
            format!(" | equipment {:?}", record.description)
        };
        writeln!(
            out,
            "{} / {} / {} | source vehicle {:?} | author {:?} | version {} | {status}{equipment}\n  {}",
            record.target.game,
            record.target.vehicle.text(),
            record.target.page.text(),
            source_vehicle,
            record.alias,
            record.remote_version,
            root.join(&record.file).display()
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;

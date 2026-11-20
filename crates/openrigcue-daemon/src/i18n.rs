//! Translations of the web UI.
//!
//! The translations are `locales/<lang>.json` files, one per language, with English as the fallback.
//! The files for all eight languages of the menu are built in ([`BUILTIN_LOCALES`],
//! `assets/locales/` of this crate); a file of the same name in the `locales/` folder of the configuration
//! directory is laid over it key by key ([`LocaleFiles::User`]). [`Translator`] holds the lookup rules: a file is loaded only when
//! a lookup misses, and a key no file defines translates to itself. The help pages of the
//! Instructions window are translated line by line ([`Translator::translate_html`]); the
//! game setup guides are translated by [`BUILTIN_HELP_LOCALES`]. Widget
//! texts are translated in the daemon's UI through
//! [`Translator::translate`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// The language codes, in menu order.
pub const LANGUAGE_CODES: [&str; 8] = ["EN", "ES", "IT", "FR", "PT", "DE", "JA", "ZH"];

/// The stored language value for a session locale such as `de_DE.UTF-8`, when it is one of the UI languages.
pub fn language_of_locale(locale: &str) -> Option<String> {
    let code = locale.split(['_', '.', '@']).next().unwrap_or_default();
    LANGUAGE_CODES.contains(&code.to_uppercase().as_str()).then(|| code.to_lowercase())
}

/// The UI language of the session: the first non-empty of `LC_ALL`, `LC_MESSAGES` and `LANG`, as `lookup`
/// answers them.
pub fn session_language(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(|name| lookup(name).filter(|value| !value.is_empty()))
        .and_then(|locale| language_of_locale(&locale))
}

/// The texts that are never translated, before the game names are appended.
pub const EXCLUDED_STRINGS: [&str; 8] = [
    "Fanalab",
    "Simucube 2 True Drive Paddock",
    "CrewChief",
    "DOF REALITY",
    "PROSIMU",
    "PT-ACTUATOR",
    "THANOS",
    "VNM SIMULATION",
];

/// The plural keys: an object made only of these stays one value.
const PLURALS: [&str; 4] = ["one", "few", "many", "zero"];

/// How deep static references may refer to each other before the lookup gives up (a reference
/// cycle ends there and the text is shown untranslated).
const MAX_REFERENCE_DEPTH: usize = 64;

/// Why a lookup failed; a failed lookup shows the key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LookupError {
    /// A file that cannot be read, is not UTF-8 or JSON, or whose root is not an object.
    #[error("cannot load {0}")]
    File(String),
    /// A static reference to a missing key, or a reference cycle.
    #[error("invalid static reference in {0}")]
    StaticRef(String),
    /// A `%` that does not start a valid placeholder.
    #[error("invalid placeholder in {0}")]
    Placeholder(String),
    /// A value that is not text or a list (number, boolean, null).
    #[error("value of {0} is not text")]
    NotText(String),
}

/// The `locales/<lang>.json` files (`assets/locales/` of this crate), built into the program: one per
/// language of the menu, with the UI texts and the help-page lines. SRS hardware
/// product names and game-side names in help steps appear as those products show them.
/// `assets/locales/hints.csv` ships next to them; nothing reads it at run time.
pub const BUILTIN_LOCALES: &[(&str, &str)] = &[
    ("de.json", include_str!("../assets/locales/de.json")),
    ("en.json", include_str!("../assets/locales/en.json")),
    ("es.json", include_str!("../assets/locales/es.json")),
    ("fr.json", include_str!("../assets/locales/fr.json")),
    ("it.json", include_str!("../assets/locales/it.json")),
    ("ja.json", include_str!("../assets/locales/ja.json")),
    ("pt.json", include_str!("../assets/locales/pt.json")),
    ("zh.json", include_str!("../assets/locales/zh.json")),
];

/// Translations of the game setup guides (`assets/help/`), one file
/// per non-English language of the menu (`assets/locales/help/<lang>.json`, keyed by the exact English line as
/// [`Translator::translate_html`] looks it up). The built-in loader merges each into the language's root file
/// when that file is loaded; a key the root file already defines keeps the root file's value. They are not part
/// of the `locales/` tree, so the folder lookup rules do not apply to them; a key of the user's file wins over
/// them as over any other built-in text.
pub const BUILTIN_HELP_LOCALES: &[(&str, &str)] = &[
    ("de.json", include_str!("../assets/locales/help/de.json")),
    ("es.json", include_str!("../assets/locales/help/es.json")),
    ("fr.json", include_str!("../assets/locales/help/fr.json")),
    ("it.json", include_str!("../assets/locales/help/it.json")),
    ("ja.json", include_str!("../assets/locales/help/ja.json")),
    ("pt.json", include_str!("../assets/locales/help/pt.json")),
    ("zh.json", include_str!("../assets/locales/help/zh.json")),
];

/// The values the help pages substitute before translation (`games::instructions`).
const HELP_VALUES: [&str; 2] = ["^ip^", "^port^"];

/// Where the `locales/` files are read from.
pub enum LocaleFiles {
    /// No locale files at all: every key translates to itself.
    None,
    /// The built-in files ([`BUILTIN_LOCALES`]).
    Builtin,
    /// A `locales/` folder on disk (tests of the lookup rules).
    Folder(PathBuf),
    /// The built-in files with the user's `locales/` folder (in the configuration directory) laid over them,
    /// what the service uses: a key of `<code>.json` in that folder wins, every other key is the built-in one
    /// (so a file written for an older version hides no text added since). A file there that cannot be read as
    /// a JSON object is ignored with a warning. The folder is read when a language is first looked up, so an
    /// edit takes effect at the next start; nothing is ever written there.
    User(PathBuf),
}

impl LocaleFiles {
    /// Whether `relative` is a file below `locales/`.
    fn is_file(&self, relative: &str) -> bool {
        match self {
            LocaleFiles::None => false,
            LocaleFiles::Builtin | LocaleFiles::User(_) => builtin(relative).is_some(),
            LocaleFiles::Folder(dir) => dir.join(relative).is_file(),
        }
    }

    /// Whether `relative` is a folder below `locales/`.
    fn is_dir(&self, relative: &str) -> bool {
        let builtin_dir = || {
            let prefix = format!("{}/", relative.to_ascii_lowercase());
            BUILTIN_LOCALES.iter().any(|(name, _)| name.starts_with(&prefix))
        };
        match self {
            LocaleFiles::None => false,
            LocaleFiles::Builtin | LocaleFiles::User(_) => builtin_dir(),
            LocaleFiles::Folder(dir) => dir.join(relative).is_dir(),
        }
    }

    /// The file read as UTF-8 text.
    fn read(&self, relative: &str) -> Result<String, LookupError> {
        let bytes = match self {
            LocaleFiles::None => None,
            LocaleFiles::Builtin | LocaleFiles::User(_) => builtin(relative).map(|text| text.as_bytes().to_vec()),
            LocaleFiles::Folder(dir) => std::fs::read(dir.join(relative)).ok(),
        };
        let bytes = bytes.ok_or_else(|| LookupError::File(relative.to_owned()))?;
        String::from_utf8(bytes).map_err(|_| LookupError::File(relative.to_owned()))
    }

    /// The texts of the user's file laid over the built-in file `relative` (nested objects flattened with `.`,
    /// values that are not text left out); empty without a user folder, without that file, and, with a logged
    /// warning naming the file, for one that is not a JSON object.
    fn user_texts(&self, relative: &str) -> Vec<(String, String)> {
        let LocaleFiles::User(dir) = self else { return Vec::new() };
        let path = dir.join(relative.to_ascii_lowercase());
        if !path.is_file() {
            return Vec::new();
        }
        let parsed = std::fs::read(&path)
            .map_err(|error| error.to_string())
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|error| error.to_string()))
            .and_then(|value| match value {
                serde_json::Value::Object(map) => Ok(map),
                _ => Err("the file is not a JSON object".to_owned()),
            });
        match parsed {
            Ok(map) => {
                let mut texts = Vec::new();
                flatten_texts(&map, "", &mut texts);
                texts
            }
            Err(error) => {
                tracing::warn!(file = %path.display(), %error, "ignoring the user's locale file; the built-in texts are used");
                Vec::new()
            }
        }
    }
}

/// The text values of a JSON object, those of nested objects under their dotted prefix.
fn flatten_texts(map: &serde_json::Map<String, serde_json::Value>, prefix: &str, texts: &mut Vec<(String, String)>) {
    for (key, value) in map {
        let full_key = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
        match value {
            serde_json::Value::Object(inner) => flatten_texts(inner, &full_key, texts),
            serde_json::Value::String(text) => texts.push((full_key, text.clone())),
            _ => {}
        }
    }
}

/// A built-in locale file by its path below `locales/` (case-insensitive, as Windows file names are).
fn builtin(relative: &str) -> Option<&'static str> {
    let wanted = relative.to_ascii_lowercase();
    BUILTIN_LOCALES.iter().find(|(name, _)| *name == wanted).map(|(_, text)| *text)
}

/// The help-guide translations merged into the built-in root file `relative`.
fn builtin_help(relative: &str) -> Option<&'static str> {
    let wanted = relative.to_ascii_lowercase();
    BUILTIN_HELP_LOCALES.iter().find(|(name, _)| *name == wanted).map(|(_, text)| *text)
}

/// `<open>text<close>` of a help-page line: split at its first `>` and its last `<`.
fn line_parts(line: &str) -> Option<(&str, &str, &str)> {
    line.split_once('>').and_then(|(open, rest)| rest.rsplit_once('<').map(|(inner, close)| (open, inner, close)))
}

/// The texts of the built-in help pages that hold a substituted value (`^ip^`, `^port^`), as
/// [`Translator::translate_html`] sees them before the substitution.
fn help_templates() -> &'static [String] {
    static TEMPLATES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    TEMPLATES.get_or_init(|| {
        let mut templates: Vec<String> = Vec::new();
        for name in crate::help_pages::names() {
            let page = crate::help_pages::page(name).unwrap_or_default().replace("&gt;", ">").replace("&nbsp;", " ");
            for line in page.split('\n') {
                if let Some((_, inner, _)) = line_parts(line) {
                    if HELP_VALUES.iter().any(|value| inner.contains(value)) && !templates.iter().any(|t| t == inner) {
                        templates.push(inner.to_owned());
                    }
                }
            }
        }
        templates
    })
}

/// The values `text` holds where `template` has `^ip^` / `^port^` (each literal part matched in order, a value
/// ending at the next literal part, the last literal part ending the text), with the number of literal bytes
/// matched; `None` when the text is not that template filled in.
fn match_template(template: &str, text: &str) -> Option<(Vec<(&'static str, String)>, usize)> {
    let mut parts: Vec<(bool, &str)> = Vec::new();
    let mut rest = template;
    while let Some((at, value)) =
        HELP_VALUES.iter().filter_map(|value| rest.find(value).map(|at| (at, *value))).min_by_key(|(at, _)| *at)
    {
        parts.push((false, &rest[..at]));
        parts.push((true, value));
        rest = &rest[at + value.len()..];
    }
    parts.push((false, rest));
    let literal_len: usize = parts.iter().filter(|(value, _)| !value).map(|(_, part)| part.len()).sum();
    let mut values: Vec<(&'static str, String)> = Vec::new();
    let mut pending: Option<&'static str> = None;
    let mut position = 0;
    let last = parts.len() - 1;
    for (index, (is_value, part)) in parts.iter().enumerate() {
        if *is_value {
            if pending.is_some() {
                return None;
            }
            pending = HELP_VALUES.iter().copied().find(|value| value == part);
            continue;
        }
        let at = match pending {
            None if index == last => {
                (text.len() == position + part.len() && text[position..].starts_with(part)).then_some(position)?
            }
            None => text[position..].starts_with(part).then_some(position)?,
            Some(_) if index == last => {
                text.ends_with(part).then(|| text.len() - part.len()).filter(|at| *at >= position)?
            }
            Some(_) => position + text[position..].find(part)?,
        };
        if let Some(value) = pending.take() {
            let captured = text[position..at].to_owned();
            if values.iter().any(|(name, held)| *name == value && *held != captured) {
                return None;
            }
            values.push((value, captured));
        }
        position = at + part.len();
    }
    Some((values, literal_len))
}

/// A loaded translation of one key in one locale.
#[derive(Debug, Clone, PartialEq)]
enum Entry {
    Text(String),
    /// Plural objects, lists, numbers, booleans, null: a widget cannot show them.
    Other(serde_json::Value),
}

/// The language state and the loaded translations.
pub struct Translator {
    files: LocaleFiles,
    /// The selected language code (upper case).
    selected: String,
    /// The lookup locale.
    locale: String,
    /// The fallback locale.
    fallback: Option<String>,
    container: HashMap<String, BTreeMap<String, Entry>>,
    /// Insertion order of the keys of each locale (for the static-reference pass).
    order: HashMap<String, Vec<String>>,
    /// Files whose content was parsed once.
    loaded: HashSet<String>,
}

impl Translator {
    /// The start-up state: the locale is `APP.language` as configured, the fallback is `en` (dropped when it
    /// equals the locale), and the selected language is `APP.language` upper-cased (`EN` when empty).
    /// [`Translator::set_language`] runs only for menu and language selections.
    pub fn new(files: LocaleFiles, configured: &str) -> Translator {
        let mut translator = Translator {
            files,
            selected: if configured.is_empty() { "EN".to_owned() } else { configured.to_uppercase() },
            locale: String::new(),
            fallback: None,
            container: HashMap::new(),
            order: HashMap::new(),
            loaded: HashSet::new(),
        };
        translator.set_config_locale(configured);
        translator.fallback = Some("en".to_owned());
        if translator.locale == "en" {
            translator.fallback = None;
        }
        translator
    }

    /// Reads the locale files from `files` from now on (for use before the first lookup: a file already
    /// loaded is not read again).
    pub fn set_files(&mut self, files: LocaleFiles) {
        self.files = files;
    }

    /// Sets the lookup locale: the fallback is dropped once it equals the locale.
    fn set_config_locale(&mut self, locale: &str) {
        locale.clone_into(&mut self.locale);
        if self.fallback.as_deref() == Some(locale) {
            self.fallback = None;
        }
    }

    /// Selects a language: an unknown code keeps the selected language; the locale
    /// becomes its lower case, which is also what `APP.language` stores (returned).
    pub fn set_language(&mut self, code: &str) -> String {
        if LANGUAGE_CODES.contains(&code) {
            code.clone_into(&mut self.selected);
        }
        let lower = self.selected.to_lowercase();
        self.set_config_locale(&lower);
        lower
    }

    pub fn selected(&self) -> &str {
        &self.selected
    }

    /// Translates `key`; a failed lookup gives the key itself.
    pub fn translate(&mut self, key: &str) -> String {
        self.lookup(key).unwrap_or_else(|_| key.to_owned())
    }

    /// Translates a help page for the Instructions window: `&gt;` and `&nbsp;` are replaced in the whole text, then every line of the form
    /// `<open>text<close>` (split at its first `>` and its last `<`) has its text translated; a text holding an
    /// inline `<div style="display: inline">value</div>` is looked up with the div replaced by `^var^`, and the
    /// value is put back in place of `^var^` (the div itself is dropped). Other lines are kept; every line ends
    /// with `\n`. `None` for an inline div without a closing `</div>`.
    pub fn translate_html(&mut self, html: &str) -> Option<String> {
        if html.is_empty() {
            return Some(String::new());
        }
        let html = html.replace("&gt;", ">").replace("&nbsp;", " ");
        let mut out = String::new();
        for line in html.split('\n') {
            let Some((open, inner, close)) = line_parts(line) else {
                out.push_str(line);
                out.push('\n');
                continue;
            };
            let text = if inner.contains(INLINE_DIV) {
                let key = replace_greedy(inner, "<div", "/div>", "^var^");
                let placeholder =
                    greedy_match(inner, "inline\">", "</div>")?.replace("inline\">", "").replace("</div>", "");
                self.translate(&key).replace("^var^", &placeholder)
            } else {
                let text = self.translate(inner);
                if text == inner {
                    self.translate_help_values(inner).unwrap_or(text)
                } else {
                    text
                }
            };
            out.push_str(open);
            out.push('>');
            out.push_str(&text);
            out.push('<');
            out.push_str(close);
            out.push('\n');
        }
        Some(out)
    }

    /// A built-in help line whose `^ip^` / `^port^` were filled in before translation (the guides write
    /// them in the text rather than in an inline div): the line's template is translated and the values put back.
    /// The template matching the most literal text wins; `None` when no built-in line has that form.
    fn translate_help_values(&mut self, text: &str) -> Option<String> {
        let (template, values) = help_templates()
            .iter()
            .filter_map(|template| match_template(template, text).map(|(values, len)| (template, values, len)))
            .max_by_key(|(_, _, len)| *len)
            .map(|(template, values, _)| (template, values))?;
        let mut translated = self.translate(template);
        for (name, value) in values {
            translated = translated.replace(name, &value);
        }
        Some(translated)
    }

    /// The lookup: the locale's translation (searching the files when it is not loaded yet), else the
    /// fallback's, else the key.
    fn lookup(&mut self, key: &str) -> Result<String, LookupError> {
        let locale = self.locale.clone();
        if let Some(entry) = self.entry(&locale, key) {
            return format_entry(key, &entry);
        }
        if self.search(key, &locale)? {
            let entry = self.entry(&locale, key).expect("search found it");
            return format_entry(key, &entry);
        }
        if let Some(fallback) = self.fallback.clone() {
            if self.entry(&fallback, key).is_some() || self.search(key, &fallback)? {
                let entry = self.entry(&fallback, key).expect("found in the fallback");
                return format_entry(key, &entry);
            }
        }
        Ok(key.to_owned())
    }

    fn entry(&self, locale: &str, key: &str) -> Option<Entry> {
        self.container.get(locale)?.get(key).cloned()
    }

    /// Finds the file for a key: `<locale>.json`, or, when it is missing, the same file in the
    /// sub-folders named by the key's leading `.` segments.
    fn search(&mut self, key: &str, locale: &str) -> Result<bool, LookupError> {
        let segments: Vec<&str> = key.split('.').collect();
        let namespace = &segments[..segments.len() - 1];
        self.search_dir(namespace, "", locale)?;
        Ok(self.container.get(locale).is_some_and(|c| c.contains_key(key)))
    }

    fn search_dir(&mut self, namespace: &[&str], directory: &str, locale: &str) -> Result<(), LookupError> {
        let file = join(directory, &format!("{locale}.json"));
        if self.files.is_file(&file) {
            return self.load_file(&file, locale);
        }
        let Some((first, rest)) = namespace.split_first() else { return Ok(()) };
        let sub = join(directory, first);
        if self.files.is_dir(&sub) {
            self.search_dir(rest, &sub, locale)?;
        }
        Ok(())
    }

    /// Loads a translation file: parse once, add every key (nested objects flattened with `.`, under the folder
    /// namespace), then expand the static references of the new keys.
    fn load_file(&mut self, file: &str, locale: &str) -> Result<(), LookupError> {
        if self.loaded.contains(file) {
            return Ok(());
        }
        let text = self.files.read(file)?;
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|_| LookupError::File(file.to_owned()))?;
        self.loaded.insert(file.to_owned());
        let serde_json::Value::Object(map) = value else { return Err(LookupError::File(file.to_owned())) };
        let namespace = Path::new(file).parent().map(|p| p.to_string_lossy().replace('/', ".")).unwrap_or_default();
        let mut added = Vec::new();
        self.add_dict(&map, &namespace, locale, &mut added);
        if let (LocaleFiles::Builtin | LocaleFiles::User(_), Some(help)) = (&self.files, builtin_help(file)) {
            let help: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(help).map_err(|_| LookupError::File(file.to_owned()))?;
            for (key, value) in help {
                let defined = self.container.get(locale).is_some_and(|keys| keys.contains_key(&key));
                if let (false, serde_json::Value::String(text)) = (defined, value) {
                    self.add(locale, &key, Entry::Text(text));
                    added.push(key);
                }
            }
        }
        // The user's texts replace the built-in ones of the same key; a reference of theirs that cannot be
        // expanded stays as written and does not fail the built-in keys.
        let user = self.files.user_texts(file);
        let user_keys: Vec<String> = user.iter().map(|(key, _)| key.clone()).collect();
        for (key, text) in user {
            self.add(locale, &key, Entry::Text(text));
        }
        for key in added {
            self.expand_static_refs(&key, locale, 0)?;
        }
        for key in user_keys {
            if let Err(error) = self.expand_static_refs(&key, locale, 0) {
                tracing::warn!(file, key, %error, "a text of the user's locale file is shown as written");
            }
        }
        Ok(())
    }

    /// Adds the keys of one JSON object, nested objects flattened under their dotted prefix.
    fn add_dict(
        &mut self,
        map: &serde_json::Map<String, serde_json::Value>,
        namespace: &str,
        locale: &str,
        added: &mut Vec<String>,
    ) {
        for (key, value) in map {
            let full_key = if namespace.is_empty() { key.clone() } else { format!("{namespace}.{key}") };
            match value {
                serde_json::Value::Object(inner)
                    if !(inner.keys().all(|k| PLURALS.contains(&k.as_str())) && !inner.is_empty()) =>
                {
                    self.add_dict(inner, &full_key, locale, added);
                }
                serde_json::Value::String(text) => {
                    self.add(locale, &full_key, Entry::Text(text.clone()));
                    added.push(full_key);
                }
                other => {
                    self.add(locale, &full_key, Entry::Other(other.clone()));
                    added.push(full_key);
                }
            }
        }
    }

    fn add(&mut self, locale: &str, key: &str, entry: Entry) {
        let container = self.container.entry(locale.to_owned()).or_default();
        if container.insert(key.to_owned(), entry).is_none() {
            self.order.entry(locale.to_owned()).or_default().push(key.to_owned());
        }
    }

    /// Expands the static references of one key: `%.a.b` / `%{.a.b}` replaced by the referenced key's
    /// translation, tried as `a.b`, then prefixed with the key's own leading segments.
    fn expand_static_refs(&mut self, key: &str, locale: &str, depth: usize) -> Result<(), LookupError> {
        if depth > MAX_REFERENCE_DEPTH {
            return Err(LookupError::StaticRef(key.to_owned()));
        }
        let entry = self.entry(locale, key).ok_or_else(|| LookupError::StaticRef(key.to_owned()))?;
        let Entry::Text(template) = entry else {
            // Anything that is not text or an object is iterated: lists pass, scalars are an error.
            return match entry {
                Entry::Other(serde_json::Value::Array(_) | serde_json::Value::Object(_)) => Ok(()),
                _ => Err(LookupError::NotText(key.to_owned())),
            };
        };
        let path: Vec<String> = key.split('.').map(str::to_owned).collect();
        let mut out = String::new();
        let mut rest = template.as_str();
        while let Some(at) = rest.find('%') {
            out.push_str(&rest[..at]);
            let after = &rest[at + 1..];
            if let Some(stripped) = after.strip_prefix('%') {
                out.push_str("%%");
                rest = stripped;
            } else if let Some(len) = static_name_len(after) {
                let reference = &after[..len];
                out.push_str(&self.resolve_reference(reference, &path, locale, depth)?);
                rest = &after[len..];
            } else if let Some(len) =
                after.strip_prefix('{').and_then(|b| static_name_len(b).filter(|&n| b[n..].starts_with('}')))
            {
                let reference = &after[1..=len];
                out.push_str(&self.resolve_reference(reference, &path, locale, depth)?);
                rest = &after[len + 2..];
            } else {
                out.push('%');
                rest = after;
            }
        }
        out.push_str(rest);
        self.container.get_mut(locale).expect("entry exists").insert(key.to_owned(), Entry::Text(out));
        Ok(())
    }

    /// Resolves one static reference to the referenced key's translation.
    fn resolve_reference(
        &mut self,
        reference: &str,
        path: &[String],
        locale: &str,
        depth: usize,
    ) -> Result<String, LookupError> {
        let mut full_key = reference.trim_start_matches('.').to_owned();
        for i in 1..=path.len() {
            if self.entry(locale, &full_key).is_some() {
                self.expand_static_refs(&full_key, locale, depth + 1)?;
                return self.reference_text(&full_key, locale);
            }
            full_key = format!("{}{reference}", path[..i].join("."));
        }
        let absolute = reference.trim_start_matches('.').to_owned();
        if self.search(&absolute, locale)? {
            return self.reference_text(&absolute, locale);
        }
        Err(LookupError::StaticRef(reference.to_owned()))
    }

    fn reference_text(&self, key: &str, locale: &str) -> Result<String, LookupError> {
        match self.entry(locale, key) {
            Some(Entry::Text(text)) => Ok(text),
            _ => Err(LookupError::NotText(key.to_owned())),
        }
    }
}

/// The inline value holder of a help-page line.
const INLINE_DIV: &str = "<div style=\"display: inline\">";

/// Where the regular expression `<start>.*<end>` (greedy, one line) first matches: from the first `start` to the
/// last `end` after it.
fn greedy_span(text: &str, start: &str, end: &str) -> Option<(usize, usize)> {
    let from = text.find(start)?;
    let after = from + start.len();
    let to = after + text[after..].rfind(end)? + end.len();
    Some((from, to))
}

/// The text of the first greedy `<start>.*<end>` match.
fn greedy_match<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    greedy_span(text, start, end).map(|(from, to)| &text[from..to])
}

/// Replaces the greedy `<start>.*<end>` match with `with`: the greedy match is the only one (nothing after it can match again).
fn replace_greedy(text: &str, start: &str, end: &str, with: &str) -> String {
    match greedy_span(text, start, end) {
        Some((from, to)) => format!("{}{with}{}", &text[..from], &text[to..]),
        None => text.to_owned(),
    }
}

fn join(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        name.to_owned()
    } else {
        format!("{directory}/{name}")
    }
}

/// A Unicode word character or `-`: the name characters of the placeholder patterns.
fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

fn name_len(text: &str) -> usize {
    text.char_indices().find(|(_, c)| !is_name_char(*c)).map_or(text.len(), |(i, _)| i)
}

/// The length of a static reference name, `(\.(\w|-)+)+`, at the start of `text`.
fn static_name_len(text: &str) -> Option<usize> {
    let mut at = 0;
    while text[at..].starts_with('.') {
        let n = name_len(&text[at + 1..]);
        if n == 0 {
            break;
        }
        at += 1 + n;
    }
    (at > 0).then_some(at)
}

/// The length of a placeholder name, `(\w|-)+(\([^()]*\))?`, at the start of `text`.
fn translation_name_len(text: &str) -> Option<usize> {
    let n = name_len(text);
    if n == 0 {
        return None;
    }
    let rest = &text[n..];
    if let Some(inner) = rest.strip_prefix('(') {
        if let Some(close) = inner.find(['(', ')']) {
            if inner[close..].starts_with(')') {
                return Some(n + 1 + close + 1);
            }
        }
    }
    Some(n)
}

/// Formats a translation without arguments: every placeholder stays as
/// written, `%%` becomes `%`, and any other `%` is an error (the lookup then returns the key).
fn format_entry(key: &str, entry: &Entry) -> Result<String, LookupError> {
    let Entry::Text(template) = entry else { return Err(LookupError::NotText(key.to_owned())) };
    let mut out = String::new();
    let mut rest = template.as_str();
    while let Some(at) = rest.find('%') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(stripped) = after.strip_prefix('%') {
            out.push('%');
            rest = stripped;
        } else if let Some(len) = translation_name_len(after) {
            out.push('%');
            out.push_str(&after[..len]);
            rest = &after[len..];
        } else if let Some(len) =
            after.strip_prefix('{').and_then(|b| translation_name_len(b).filter(|&n| b[n..].starts_with('}')))
        {
            out.push('%');
            out.push_str(&after[..len + 2]);
            rest = &after[len + 2..];
        } else {
            return Err(LookupError::Placeholder(key.to_owned()));
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// Separates the parts of a sentence that carries its values ([`sentence`]).
const SENTENCE_MARK: char = '\u{1f}';

/// A text for display that holds values, as one string: the sentence with `^name^` parts, then each name and
/// its value. The parts of the service that have no translator at hand (the VR setup, the sound output, the
/// import) give their failures this way; where a failure is handed to the UI the sentence is looked up and the
/// values are filled in ([`Translator::translate_sentence`]), and logs and the HTTP API get the English text
/// ([`plain`]). A value is shown as it is: a path, a program's own output, the system's error text.
pub fn sentence(text: &str, values: &[(&str, &str)]) -> String {
    let mut out = text.to_owned();
    for (name, value) in values {
        out.push(SENTENCE_MARK);
        out.push_str(name);
        out.push(SENTENCE_MARK);
        out.push_str(&value.replace(SENTENCE_MARK, " "));
    }
    out
}

/// The sentence and the values of a text made by [`sentence`]; a text without values is its own sentence.
fn sentence_parts(message: &str) -> (&str, Vec<(&str, &str)>) {
    let mut parts = message.split(SENTENCE_MARK);
    let text = parts.next().unwrap_or_default();
    let mut values = Vec::new();
    while let (Some(name), Some(value)) = (parts.next(), parts.next()) {
        values.push((name, value));
    }
    (text, values)
}

/// `text` with every `^name^` part replaced by its value.
fn fill(text: &str, values: &[(&str, &str)]) -> String {
    values.iter().fold(text.to_owned(), |text, (name, value)| text.replace(&format!("^{name}^"), value))
}

/// The English text of a message that may have been made by [`sentence`].
pub fn plain(message: &str) -> String {
    let (text, values) = sentence_parts(message);
    fill(text, &values)
}

impl Translator {
    /// Translates a message that may have been made by [`sentence`]: the sentence is looked up and its values
    /// are filled in; any other text is looked up as it is.
    pub fn translate_sentence(&mut self, message: &str) -> String {
        let (text, values) = sentence_parts(message);
        fill(&self.translate(text), &values)
    }
}

/// The texts that are never translated: the fixed names, then the Games page entry names, then
/// the catalog's game names.
pub fn excluded_strings() -> Vec<String> {
    let mut excluded: Vec<String> = EXCLUDED_STRINGS.iter().map(|s| (*s).to_owned()).collect();
    excluded.extend(openrigcue_config::patch::GAME_ENTRIES.iter().map(|e| e.name.to_owned()));
    excluded.extend(openrigcue_config::catalog::Catalog::builtin().games().iter().map(|g| g.name.clone()));
    excluded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_locale_selects_a_ui_language() {
        assert_eq!(language_of_locale("de_DE.UTF-8").as_deref(), Some("de"));
        assert_eq!(language_of_locale("pt_BR").as_deref(), Some("pt"));
        assert_eq!(language_of_locale("ja").as_deref(), Some("ja"));
        assert_eq!(language_of_locale("zh_CN.UTF-8@pinyin").as_deref(), Some("zh"));
        assert_eq!(language_of_locale("nl_NL.UTF-8"), None);
        assert_eq!(language_of_locale("C"), None);
        assert_eq!(language_of_locale(""), None);
        let session = |values: &'static [(&'static str, &'static str)]| {
            session_language(move |name| {
                values.iter().find(|(key, _)| *key == name).map(|(_, value)| (*value).to_owned())
            })
        };
        assert_eq!(session(&[("LANG", "fr_FR.UTF-8")]).as_deref(), Some("fr"));
        assert_eq!(session(&[("LC_ALL", ""), ("LC_MESSAGES", "it_IT"), ("LANG", "fr_FR")]).as_deref(), Some("it"));
        assert_eq!(session(&[("LC_ALL", "es_ES"), ("LANG", "fr_FR")]).as_deref(), Some("es"));
        assert_eq!(session(&[("LC_ALL", "C"), ("LANG", "fr_FR")]), None);
        assert_eq!(session(&[]), None);
    }

    fn folder(files: &[(&str, &str)]) -> (tempfile::TempDir, LocaleFiles) {
        let tmp = tempfile::tempdir().unwrap();
        for (name, text) in files {
            let path = tmp.path().join("locales").join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let files = LocaleFiles::Folder(tmp.path().join("locales"));
        (tmp, files)
    }

    /// A key of `<locale>.json` is translated, a missing one falls back to
    /// `en.json`, anything else stays as written; without locale files every text stays as written.
    #[test]
    fn lookup_locale_then_fallback_then_key() {
        let (_tmp, files) = folder(&[("de.json", r#"{"Save": "Speichern"}"#), ("en.json", r#"{"Cancel": "Cancel!"}"#)]);
        let mut t = Translator::new(files, "de");
        assert_eq!(t.translate("Save"), "Speichern");
        assert_eq!(t.translate("Cancel"), "Cancel!");
        assert_eq!(t.translate("Reset"), "Reset");
        let mut none = Translator::new(LocaleFiles::None, "de");
        assert_eq!(none.translate("Save"), "Save");
    }

    /// Setting the locale to the fallback ('en') clears the fallback for good, so after the
    /// language was English once, a missing German key is no longer looked up in `en.json`.
    #[test]
    fn fallback_is_cleared_once_the_locale_was_english() {
        let (_tmp, files) = folder(&[("de.json", r#"{"Save": "Speichern"}"#), ("en.json", r#"{"Cancel": "Cancel!"}"#)]);
        let mut t = Translator::new(files, "en");
        assert_eq!(t.set_language("DE"), "de");
        assert_eq!(t.translate("Save"), "Speichern");
        assert_eq!(t.translate("Cancel"), "Cancel");
    }

    /// Selecting a language: unknown codes keep the selected language; the stored value is lower case;
    /// an empty configuration starts in EN.
    #[test]
    fn set_language_rules() {
        let mut t = Translator::new(LocaleFiles::None, "");
        assert_eq!(t.selected(), "EN");
        assert_eq!(t.set_language("FR"), "fr");
        assert_eq!(t.set_language("XX"), "fr");
        assert_eq!(t.selected(), "FR");
        let t = Translator::new(LocaleFiles::None, "pt");
        // Start-up takes the configured value as is; only `set_language` lower-cases it.
        assert_eq!(Translator::new(LocaleFiles::None, "DE").locale, "DE");
        assert_eq!(t.selected(), "PT");
    }

    /// Formatting without arguments: `%%` → `%`, named and braced placeholders kept, a lone `%`
    /// fails and the lookup returns the key.
    #[test]
    fn placeholder_rules() {
        let (_tmp, files) = folder(&[(
            "de.json",
            r#"{"a": "100%% sicher", "b": "Hallo %name und %{x} %f(1)", "c": "50% Leistung", "d": "Ende %"}"#,
        )]);
        let mut t = Translator::new(files, "de");
        assert_eq!(t.translate("a"), "100% sicher");
        assert_eq!(t.translate("b"), "Hallo %name und %{x} %f(1)");
        assert_eq!(t.translate("c"), "c");
        assert_eq!(t.translate("d"), "d");
    }

    /// Loading: nested objects become dotted keys, plural objects and other values are not text
    /// (shown untranslated); `%.key` references are replaced.
    #[test]
    fn nested_keys_plurals_and_static_references() {
        let (_tmp, files) = folder(&[(
            "fr.json",
            r#"{"menu": {"save": "Enregistrer"}, "n": {"one": "un", "many": "plusieurs"}, "ref": "%.menu.save !", "keep": "%%.menu.save"}"#,
        )]);
        let mut t = Translator::new(files, "fr");
        assert_eq!(t.translate("menu.save"), "Enregistrer");
        assert_eq!(t.translate("n"), "n");
        assert_eq!(t.translate("ref"), "Enregistrer !");
        assert_eq!(t.translate("keep"), "%.menu.save");
    }

    /// Expanding static references fails on a number: that lookup shows its key; the keys
    /// were added before, the file is memoised, so later lookups find them (unexpanded after the number).
    #[test]
    fn number_value_fails_the_first_lookup_only() {
        let (_tmp, files) = folder(&[("pt.json", r#"{"a": "um", "x": 5, "ref": "%.a"}"#)]);
        let mut t = Translator::new(files, "pt");
        assert_eq!(t.translate("a"), "a");
        assert_eq!(t.translate("a"), "um");
        assert_eq!(t.translate("x"), "x");
        assert_eq!(t.translate("ref"), "ref");
    }

    /// A file that is not valid JSON makes every lookup of that locale fail (the key is shown, the fallback is
    /// not consulted); a missing file lets the fallback answer.
    #[test]
    fn broken_locale_file_shows_keys() {
        let (_tmp, files) = folder(&[("it.json", "{nope"), ("en.json", r#"{"Save": "Save!"}"#)]);
        let mut t = Translator::new(files, "it");
        assert_eq!(t.translate("Save"), "Save");
        let (_tmp2, files) = folder(&[("en.json", r#"{"Save": "Save!"}"#)]);
        let mut t = Translator::new(files, "it");
        assert_eq!(t.translate("Save"), "Save!");
    }

    /// The user's `locales/` folder is laid over the built-in files key by key: a key of the user's file wins,
    /// every key it lacks is the built-in one, a file that does not parse is ignored (the built-in texts are
    /// used), and a language without a file there, or no folder at all, is the built-in file alone.
    #[test]
    fn user_folder_file_overlays_the_builtin_one() {
        let builtin = |code: &str, key: &str| Translator::new(LocaleFiles::Builtin, code).translate(key);
        assert_ne!(builtin("de", "Save"), "Save");
        assert_ne!(builtin("de", "Cancel"), "Cancel");
        let (tmp, _) = folder(&[
            ("de.json", r#"{"Save": "Sichern!", "Only Mine": "Nur meins", "menu": {"x": "Mein X"}, "Cancel": 5}"#),
            ("it.json", "{nope"),
            ("es.json", r#"["Save"]"#),
            ("en.json", r#"{"Wind": "Breeze"}"#),
        ]);
        let user = || LocaleFiles::User(tmp.path().join("locales"));
        let mut t = Translator::new(user(), "de");
        // A key of the user's file wins, also one the built-in file does not have.
        assert_eq!(t.translate("Save"), "Sichern!");
        assert_eq!(t.translate("Only Mine"), "Nur meins");
        assert_eq!(t.translate("menu.x"), "Mein X");
        // Keys the user's file lacks, and one whose value there is not text, are the built-in ones.
        assert_eq!(t.translate("Reset"), builtin("de", "Reset"));
        assert_ne!(t.translate("Reset"), "Reset");
        assert_eq!(t.translate("Cancel"), builtin("de", "Cancel"));
        // The guide translations merged into the built-in file stay.
        let guide = "Open Game Options → Settings and find UDP Telemetry Settings.";
        assert_ne!(t.translate(guide), guide);
        // The user's English file is laid over the fallback the same way.
        assert_eq!(t.translate("Motionbox"), builtin("de", "Motionbox"));
        let mut en = Translator::new(user(), "en");
        assert_eq!(en.translate("Wind"), "Breeze");
        assert_eq!(en.translate("Motionbox"), "Motion");
        // A file that is not JSON, or not a JSON object, is ignored.
        for code in ["it", "es"] {
            let mut t = Translator::new(user(), code);
            assert_eq!(t.translate("Save"), builtin(code, "Save"), "{code}");
            assert_ne!(t.translate("Save"), "Save", "{code}");
        }
        // A language without a file in the folder, and a folder that does not exist: built-in only.
        let mut t = Translator::new(user(), "fr");
        assert_eq!(t.translate("Save"), builtin("fr", "Save"));
        let absent = tmp.path().join("absent");
        let mut t = Translator::new(LocaleFiles::User(absent.clone()), "de");
        assert_eq!(t.translate("Save"), builtin("de", "Save"));
        assert!(!absent.exists(), "nothing is created in the configuration directory");
    }

    /// The texts of the sound device row, the downloaded-profiles window, the VR panel and its status readout,
    /// the VR setup rows, the help window and a sample of the dialogs are translated in every non-English
    /// language, and the confirmation question keeps its four `^name^` parts.
    #[test]
    fn builtin_languages_translate_the_added_texts() {
        let question = "Apply this ^type^ profile by ^author^ to ^game^ — ^vehicle^? Other settings stay unchanged.";
        for code in LANGUAGE_CODES.iter().filter(|code| **code != "EN") {
            let mut t = Translator::new(LocaleFiles::Builtin, &code.to_lowercase());
            for key in [
                "Sound Device",
                "Use SRS Sound Samples",
                "Downloaded Profiles",
                "All vehicles",
                "Select a profile, then click Apply. Use Save to keep your changes or Cancel to discard them.",
                "The selected game’s tuning controls are still loading. Try again after they refresh.",
                "VR Compensation",
                "Centre of Rotation",
                "Sensor Reading (°)",
                "Use Seated Height",
                "Recenter",
                "Uninstall",
                "Checking installation",
                question,
                // The VR status readout, the sound device failure, the App page's help and the Games page's
                // refusal while Steam runs.
                "Writing",
                "Waiting",
                "Stopped",
                "Live",
                "Stale",
                "No sample",
                "No status file",
                "Status unavailable: ^error^",
                "sample ^age^ ms old",
                "sway: ^sway^  surge: ^surge^  heave: ^heave^  yaw: ^yaw^  roll: ^roll^  pitch: ^pitch^",
                "Could not open the selected sound device: ^error^",
                "Help",
                "OpenRigCue Help",
                "OpenRigCue ^version^ — motion cueing and sim effects",
                "Steam Running",
                "Steam is running, so OpenRigCue cannot set the game up. Close Steam, then try again.",
                // Window titles, table headers, buttons and dropdown entries.
                "Edit",
                "Next",
                "Routine Configuration",
                "Stop Recording",
                "Actuator Speed",
                "Semi Hard",
                "Vehicle: ^vehicle^",
                "Game: ^game^",
                "Game Telemetry Not Detected",
                "3 Actuators (2 Front + 1 Rear) CANBus",
            ] {
                assert_ne!(t.translate(key), key, "{code}: {key}");
            }
            let translated = t.translate(question);
            for part in ["^type^", "^author^", "^game^", "^vehicle^"] {
                assert_eq!(translated.matches(part).count(), 1, "{code}: {part}");
            }
        }
    }

    /// File search: without `<locale>.json` the sub-folder named by the key's first segment is searched.
    #[test]
    fn namespaced_folder_files() {
        let (_tmp, files) = folder(&[("menu/ja.json", r#"{"save": "保存"}"#)]);
        let mut t = Translator::new(files, "ja");
        assert_eq!(t.translate("menu.save"), "保存");
    }

    /// Translation over the locale and the fallback: a value with an unescaped `%` fails and gives the key.
    #[test]
    fn locale_then_fallback() {
        let (_tmp, files) =
            folder(&[("es.json", r#"{"Save": "Guardar", "Bad": "50% x"}"#), ("en.json", r#"{"Cancel": "Cancel"}"#)]);
        let mut t = Translator::new(files, "es");
        assert_eq!(t.selected(), "ES");
        assert_eq!(t.translate("Save"), "Guardar");
        assert_eq!(t.translate("Bad"), "Bad");
        assert_eq!(t.translate("Cancel"), "Cancel");
    }

    /// The untranslated texts: the fixed names, then the Games page entries, then the catalog.
    #[test]
    fn excluded_strings_order() {
        let excluded = excluded_strings();
        assert_eq!(&excluded[..8], EXCLUDED_STRINGS);
        assert_eq!(excluded[8], "*Stand By Mode");
        assert!(excluded.iter().any(|n| n == "Assetto Corsa PS4"));
    }

    /// Out of the box the built-in `en.json` shows `Motionbox` as `Motion`;
    /// other English keys translate to themselves.
    #[test]
    fn builtin_english() {
        let mut t = Translator::new(LocaleFiles::Builtin, "EN");
        assert_eq!(t.translate("Motionbox"), "Motion");
        assert_eq!(
            t.translate("Motion is switched off on the Setup -> Hardware page"),
            "Motion is switched off on the Setup -> Hardware page"
        );
        assert_eq!(t.translate("Wind"), "Wind");
    }

    /// The `^name^` parts of a text that values are filled into, in order.
    fn placeholders(text: &str) -> Vec<&str> {
        text.split('^')
            .skip(1)
            .step_by(2)
            .filter(|name| !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase()))
            .collect()
    }

    /// The eight built-in languages: every file parses and defines every key `en.json` defines; every entry is a
    /// text whose lookup succeeds, except the older spelling of the stroke text in six files; and a text with
    /// `^name^` parts keeps each of them in every language.
    #[test]
    fn builtin_languages_cover_every_key() {
        let english: serde_json::Map<String, serde_json::Value> = serde_json::from_str(BUILTIN_LOCALES[1].1).unwrap();
        assert_eq!(BUILTIN_LOCALES[1].0, "en.json");
        let stroke = r"At 1 the stroke travel is capped at 10\%, at 100 it is capped at 1000\%";
        for code in LANGUAGE_CODES {
            let lower = code.to_lowercase();
            let mut t = Translator::new(LocaleFiles::Builtin, &lower);
            let (file, text) = BUILTIN_LOCALES.iter().find(|(name, _)| *name == format!("{lower}.json")).unwrap();
            let own: serde_json::Map<String, serde_json::Value> = serde_json::from_str(text).unwrap();
            let missing: Vec<&String> = english.keys().filter(|k| !own.contains_key(*k)).collect();
            assert!(missing.is_empty(), "{file} lacks {missing:?}");
            let fails = matches!(code, "EN" | "ES" | "IT" | "FR" | "PT" | "DE");
            assert_eq!(t.translate(stroke) == stroke, fails, "{file}");
            let failing: Vec<&String> = english.keys().filter(|key| t.lookup(key).is_err()).collect();
            assert_eq!(failing, if fails { vec![stroke] } else { Vec::<&str>::new() }, "{file}");
            for key in english.keys() {
                let mut wanted = placeholders(key);
                let translated = t.translate(key);
                let mut found = placeholders(&translated);
                wanted.sort_unstable();
                found.sort_unstable();
                assert_eq!(found, wanted, "{file}: {key} -> {translated}");
            }
        }
        let mut de = Translator::new(LocaleFiles::Builtin, "de");
        assert_eq!(de.translate("Save"), "Speichern");
        assert_eq!(de.translate("Playstation IP"), "Playstation IP", "SRS's German file keeps it English");
        let mut zh = Translator::new(LocaleFiles::Builtin, "zh");
        assert_eq!(zh.translate("No USB link to the belt"), "安全带 USB未连接");
        // The stroke text as the Motion page writes it has an entry of its own, which every language can show.
        let shown = "At 1 the stroke travel is capped at 10%, at 100 it is capped at 1000%";
        for code in LANGUAGE_CODES {
            let translated = Translator::new(LocaleFiles::Builtin, &code.to_lowercase()).translate(shown);
            assert_eq!(translated.matches('%').count(), 2, "{code}: {translated}");
            assert_eq!(translated == shown, code == "EN", "{code}");
        }
    }

    /// A sentence that carries its values: the English text for logs and the HTTP API, the translated sentence
    /// with the values in the places the translation gives them, and a text without values looked up as it is.
    #[test]
    fn sentences_carry_their_values() {
        let message = sentence("Cannot read ^path^: ^error^", &[("path", "/a/b.toml"), ("error", "Permission denied")]);
        assert_eq!(plain(&message), "Cannot read /a/b.toml: Permission denied");
        let mut de = Translator::new(LocaleFiles::Builtin, "de");
        assert_eq!(de.translate_sentence(&message), "/a/b.toml kann nicht gelesen werden: Permission denied");
        assert_eq!(de.translate_sentence("SteamVR was not found"), "SteamVR wurde nicht gefunden");
        assert_eq!(de.translate_sentence("an unknown text"), "an unknown text");
        assert_eq!(plain("an unknown text"), "an unknown text");
        let mut en = Translator::new(LocaleFiles::Builtin, "en");
        assert_eq!(en.translate_sentence(&message), plain(&message));
        // The failures the service builds this way are the English texts their errors show.
        let source = std::path::Path::new("/srs");
        assert_eq!(plain(&crate::no_srs_files(source)), "no SRS sound or firmware files found in /srs");
        let missing = crate::shakers::SoundcardError::NotFound("USB".into());
        assert_eq!(plain(&missing.message()), missing.to_string());
        let reason = sentence("audio device: ^error^", &[("error", "busy")]);
        assert_eq!(crate::shakers::SoundcardError::Init(reason).to_string(), "audio device: busy");
    }

    /// Every sentence the service builds with values (the first text of each `sentence(…)` and of the VR setup's
    /// `failed(…)`) has an entry in every language, found in the sources themselves so that a new one cannot be
    /// left out.
    #[test]
    fn every_sentence_with_values_has_an_entry_in_every_language() {
        let sources = [
            include_str!("compensation/setup.rs"),
            include_str!("compensation/publisher.rs"),
            include_str!("audio.rs"),
            include_str!("shakers.rs"),
            include_str!("lib.rs"),
        ];
        let mut sentences = Vec::new();
        for source in sources {
            for call in ["sentence(", "failed("] {
                for (at, _) in source.match_indices(call) {
                    let rest = source[at + call.len()..].trim_start();
                    let Some(rest) = rest.strip_prefix('"') else { continue };
                    let mut text = String::new();
                    let mut chars = rest.chars();
                    while let Some(c) = chars.next() {
                        match c {
                            '"' => break,
                            '\\' => text.extend(chars.next()),
                            c => text.push(c),
                        }
                    }
                    sentences.push(text);
                }
            }
        }
        assert!(sentences.len() >= 20, "{sentences:?}");
        assert!(sentences.iter().any(|text| text == "SteamVR ^operation^ failed (^status^): ^output^"));
        assert!(sentences.iter().any(|text| text == "soundcard \"^name^\" not found in driver list"));
        for code in LANGUAGE_CODES {
            let own = entries(code);
            for text in &sentences {
                assert!(own.contains_key(text), "{code}: no entry for {text:?}");
            }
        }
    }

    /// The entries of one built-in language, as the file defines them.
    fn entries(code: &str) -> serde_json::Map<String, serde_json::Value> {
        let name = format!("{}.json", code.to_lowercase());
        serde_json::from_str(BUILTIN_LOCALES.iter().find(|(file, _)| *file == name).unwrap().1).unwrap()
    }

    /// The texts the service itself produces for display have an entry in every language: the titles and
    /// messages of its notifications and alerts, the refusals of the Games page, the statuses and failures of
    /// the VR setup rows, the shortcut editor's action labels and the snapshot failure. The texts the UI fills
    /// values into are entries with `^name^` parts, and filled in in English they are the service's own texts.
    #[test]
    fn service_texts_have_an_entry_in_every_language() {
        use crate::notifications::Notifications;
        use openrigcue_config::patch::GameStatus;
        let mut texts: Vec<String> = [
            crate::sound_devices::DEFAULT_CARD_TITLE,
            crate::sound_devices::DEFAULT_CARD_MESSAGE,
            crate::hotkeys::snapshot::SNAPSHOT_ERROR,
            "Success",
            "Failed",
            "Calibration has finished",
            "Calibration did not succeed",
            "YawVR was told to center",
            "Installed",
            "Not installed",
            "Component unavailable",
            "SteamVR not found",
            "Setup failed: ^error^",
            "The Linux VR component is missing from this package. Use a complete OpenRigCue build.",
            "OpenXR registration is missing from this package",
            "OpenXR registration could not be verified",
            "SteamVR was not found",
            "SteamVR did not confirm the driver registration",
            "Bundled VR component could not be verified",
            "SteamVR did not remove the driver registration",
            "SteamVR was not found; registration cannot be changed yet.",
            "SteamVR restart command is unavailable",
            "Driver settings must be a TOML table",
            "Installation path has no parent",
            "the centre-of-rotation distances must be finite numbers",
            "4-channel (Quadraphonic)",
            "6-channel (5.1 Surround)",
            "5.1 Surround",
            "no SRS installer or installation folder given",
            "the configuration is not stored in a folder",
            "no default output device",
            "the device offers no 2, 4, 6 or 8 channel output in a usable sample format",
            "Shakers - ^label^ Required",
            "^game^ telemetry",
            "Open the ^game^ setup instructions to enable telemetry.",
            "Allow OpenRigCue to access ^devices^.",
        ]
        .map(str::to_owned)
        .to_vec();
        texts.push(crate::games::GamesError::GameRunning.to_string());
        texts.push(crate::games::GamesError::SteamRunning.to_string());
        texts.push(crate::joystick::JoystickError::NameExists.to_string());
        for group in crate::hotkeys::names::groups() {
            for action in group.actions {
                texts.push(crate::hotkeys::names::label(&action).unwrap_or_else(|| panic!("no label for {action}")));
            }
        }
        // The notifications whose texts are fixed, as the service raises them.
        let mut notifications = Notifications::default();
        notifications.check_game_config("dirt4", &GameStatus::NotPatched, false);
        notifications.device_permissions(&["/dev/ttyUSB0".into(), "/dev/ttyACM1".into()]);
        let errors = [crate::notifications::MEMORY_READ_REFUSED_TEXT.to_owned()].into_iter().collect();
        notifications.check_memory_read_refused(Some("ed"), &errors);
        let errors = [crate::notifications::PORT_IN_USE_TEXT.to_owned()].into_iter().collect();
        notifications.check_port_in_use("forzahorizon5", &errors, Some(10001), |_| Some("SimHub.exe".into()));
        for notification in &notifications.items {
            texts.push(notification.title.clone());
            if !matches!(
                notification.kind,
                crate::notifications::NotificationKind::DevicePermissions { .. }
                    | crate::notifications::NotificationKind::PortInUse { .. }
            ) {
                texts.push(notification.message.clone());
            }
        }
        let port = "No game telemetry reaches OpenRigCue because ^process^ occupies port ^port^. Close ^process^ so that OpenRigCue can receive the telemetry";
        texts.push(port.to_owned());
        for code in LANGUAGE_CODES {
            let own = entries(code);
            for text in &texts {
                // A label that starts with spaces (the rumble and haptic shortcut labels) is looked up without
                // them by the UI, which keeps the indentation.
                let found = own.contains_key(text) || own.contains_key(text.trim_start_matches(' '));
                assert!(found, "{code}: no entry for {text:?}");
            }
        }
        // In English the UI's sentences with the values filled in are the texts the service wrote.
        let mut english = Translator::new(LocaleFiles::Builtin, "en");
        let message = |kind: &str| {
            let found = notifications.items.iter().find(|n| serde_json::to_value(n).unwrap()["kind"] == kind);
            found.unwrap_or_else(|| panic!("no {kind} notification")).message.clone()
        };
        assert_eq!(
            english.translate(port).replace("^process^", "SimHub.exe").replace("^port^", "10001"),
            message("port_in_use")
        );
        assert_eq!(
            english
                .translate("Allow OpenRigCue to access ^devices^.")
                .replace("^devices^", "/dev/ttyUSB0, /dev/ttyACM1"),
            message("device_permissions")
        );
        let mut instructions = Notifications::default();
        instructions.check_game_config("forzahorizon5", &GameStatus::Instructions, false);
        let name = "Forza Horizon 5";
        assert_eq!(english.translate("^game^ telemetry").replace("^game^", name), instructions.items[0].title);
        assert_eq!(
            english.translate("Open the ^game^ setup instructions to enable telemetry.").replace("^game^", name),
            instructions.items[0].message
        );
    }

    /// Help-page translation: `&gt;`/`&nbsp;` replaced, `<open>text<close>` lines translated, an inline div looked up as
    /// `^var^` with its value put back (the div is dropped), other lines kept, every line ends with `\n`.
    #[test]
    fn translate_html_rules() {
        let (_tmp, files) = folder(&[(
            "de.json",
            r#"{"Go -> Settings": "Gehe -> Einstellungen", "Port ^var^": "Anschluss ^var^", "a b": "A B"}"#,
        )]);
        let mut t = Translator::new(files, "de");
        let page = "<ol>\n<li>Go -&gt; Settings</li>\n<li>Port <div style=\"display: inline\">20777</div></li>\n\
                    <p>a&nbsp;b</p>\nplain\n<li>Other</li>";
        assert_eq!(
            t.translate_html(page).unwrap(),
            "<ol>\n<li>Gehe -> Einstellungen</li>\n<li>Anschluss 20777</li>\n<p>A B</p>\nplain\n<li>Other</li>\n"
        );
        assert_eq!(t.translate_html("").unwrap(), "");
        assert_eq!(t.translate_html("<b>x <div style=\"display: inline\">1</b>"), None);
    }

    /// The built-in help pages go through the locale files: a line with a key is translated (the inline value put
    /// back), a line the file lacks is shown as written.
    #[test]
    fn builtin_help_pages_are_translated() {
        let mut t = Translator::new(LocaleFiles::Builtin, "fr");
        let page = crate::games::instructions(
            "f12019",
            true,
            &openrigcue_config::settings::NetworkPorts::default(),
            "10.1.2.3",
        )
        .unwrap();
        let html = t.translate_html(&page).unwrap();
        let port = openrigcue_config::settings::NetworkPorts::default().get("f12019").unwrap();
        assert!(html.contains("<h3>F1 2019 console</h3>"), "the game title stays: {html}");
        assert!(html.contains("<li>Ouvrez Game Options → Settings et repérez UDP Telemetry Settings.</li>"), "{html}");
        assert!(
            html.contains(&format!("<li>Réglez l’adresse de destination sur 10.1.2.3 et le port UDP sur {port}.</li>")),
            "the substituted address and port go back into the translated sentence: {html}"
        );
        assert!(html.contains("<li>Choisissez 2019 comme UDP Format.</li>"), "{html}");
        assert!(!html.contains("^ip^") && !html.contains("^port^") && !html.contains("^var^"), "{html}");
        // The merged guide translations keep every entry of the root fr.json.
        assert_eq!(t.translate("Games"), "Jeux");
        assert_eq!(t.translate("Aller dans Options de jeu -> Paramètres"), "Aller dans Options de jeu -> Paramètres");
        assert_eq!(t.translate("STEP 1: IN-GAME CONFIGURATION"), "STEP 1: IN-GAME CONFIGURATION");
    }

    /// Every step and note (`<li>`, `<p>`) of all 60 built-in guides is translated in the seven other menu
    /// languages, with the same address, port and other numbers, links and entities; English shows the pages as
    /// written, titles and code blocks are kept.
    #[test]
    fn every_builtin_guide_line_is_translated_in_every_language() {
        let ports = openrigcue_config::settings::NetworkPorts::default();
        let ip = "10.20.30.40";
        let mut english = Translator::new(LocaleFiles::Builtin, "en");
        let numbers = |line: &str| -> Vec<String> {
            line.split(|c: char| !(c.is_ascii_digit() || c == '.'))
                .map(|run| run.trim_matches('.').to_owned())
                .filter(|run| run.chars().any(|c| c.is_ascii_digit()))
                .collect()
        };
        let mut steps = 0;
        for code in LANGUAGE_CODES.iter().filter(|code| **code != "EN") {
            let mut t = Translator::new(LocaleFiles::Builtin, &code.to_lowercase());
            for name in crate::help_pages::names() {
                let (base, console) = name.strip_suffix("_console").map_or((name, false), |base| (base, true));
                let page = crate::games::instructions(base, console, &ports, ip).unwrap();
                let source = english.translate_html(&page).unwrap();
                assert_eq!(source, format!("{}\n", page.replace("&gt;", ">").replace("&nbsp;", " ")), "{name}");
                let translated = t.translate_html(&page).unwrap();
                let (source, translated): (Vec<&str>, Vec<&str>) =
                    (source.lines().collect(), translated.lines().collect());
                assert_eq!(source.len(), translated.len(), "{code} {name}");
                for (en, tr) in source.iter().zip(&translated) {
                    if !(en.starts_with("<li>") || en.starts_with("<p>")) {
                        continue;
                    }
                    steps += 1;
                    assert_ne!(en, tr, "{code} {name}: untranslated");
                    assert!(!tr.contains("^ip^") && !tr.contains("^port^"), "{code} {name}: {tr}");
                    for number in numbers(en) {
                        assert!(tr.contains(&number), "{code} {name}: {number} missing in {tr}");
                    }
                    for token in ["href=\"", "</a>", "&amp;", "<span>", "→"] {
                        assert_eq!(
                            en.matches(token).count(),
                            tr.matches(token).count(),
                            "{code} {name}: {token} in {tr}"
                        );
                    }
                }
            }
        }
        assert_eq!(steps, 7 * 195, "every step line of every guide in seven languages");
    }

    /// A guide line holding substituted values is translated through its template: each value is taken where the
    /// template has it, and a text that is not the template filled in is left alone.
    #[test]
    fn help_values_match_their_template() {
        let template = "Set the destination address to ^ip^ and the UDP port to ^port^.";
        let (values, _) =
            match_template(template, "Set the destination address to 1.2.3.4 and the UDP port to +020.").unwrap();
        assert_eq!(values, [("^ip^", "1.2.3.4".to_owned()), ("^port^", "+020".to_owned())]);
        assert!(match_template(template, "Set the destination address to 1.2.3.4 and the UDP port to 20").is_none());
        assert!(match_template(template, "Set the address to 1.2.3.4 and the UDP port to 20.").is_none());
        let mut de = Translator::new(LocaleFiles::Builtin, "de");
        assert_eq!(
            de.translate_html("<li>Set Data Out IP Port to <span>5300</span>.</li>").unwrap(),
            "<li>Stellen Sie Data Out IP Port auf <span>5300</span>.</li>\n"
        );
        // Not a guide line: shown as written.
        assert_eq!(de.translate_html("<li>Set the port to 5300.</li>").unwrap(), "<li>Set the port to 5300.</li>\n");
    }
}

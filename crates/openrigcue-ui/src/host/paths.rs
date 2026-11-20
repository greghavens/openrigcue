//! The UI's anchored route patterns and query handling, after the GET handler unquotes
//! the entire request target. The patterns have different anchoring rules.

use super::super::remi::client;

mod word;

pub(super) enum Route {
    Page,
    Static { namespace: String, file: String },
    Attribute { widget: String, method: String, parameters: Vec<(String, String)> },
    NoResponse,
}

fn word(value: char) -> bool {
    let value = u32::from(value);
    let end = word::WORD_RANGES.partition_point(|(start, _)| *start <= value);
    end > 0 && value <= word::WORD_RANGES[end - 1].1
}

fn word_end(value: &str) -> usize {
    value.char_indices().find(|(_, value)| !word(*value)).map_or(value.len(), |(index, _)| index)
}

fn query_pattern(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    let parts = value.strip_suffix('&').unwrap_or(value);
    parts.split('&').all(|part| {
        let Some((name, value)) = part.split_once('=') else { return false };
        name.chars().all(word) && !value.is_empty()
    })
}

fn query_parameters(target: &str) -> Vec<(String, String)> {
    // CR/LF/TAB are removed before recognizing the query and fragment.
    let target: String = target.chars().filter(|ch| !matches!(ch, '\r' | '\n' | '\t')).collect();
    let target = target.split('#').next().unwrap_or("");
    let query = target.split_once('?').map_or("", |(_, query)| query);
    let mut result: Vec<(String, String)> = Vec::new();
    for field in query.split('&') {
        let Some((key, value)) = field.split_once('=') else { continue };
        if value.is_empty() {
            continue;
        }
        let key = client::from_websocket(&key.replace('+', " "));
        let value = client::from_websocket(&value.replace('+', " "));
        if !result.iter().any(|(present, _)| present == &key) {
            result.push((key, value));
        }
    }
    result
}

pub(super) fn classify(target: &str) -> Route {
    let target = client::from_websocket(target);
    if target == "/" || target.is_empty() {
        return Route::Page;
    }
    let path = target.trim_start_matches('/');
    let first = word_end(path);
    if first == 0 {
        return Route::NoResponse;
    }
    if let Some(file) = path[first..].strip_prefix(':') {
        let allowed = |ch: char| word(ch) || "-_. $@?#£'%=()/[]!+°§^,".contains(ch);
        let end = file.char_indices().find(|(_, ch)| !allowed(*ch)).map_or(file.len(), |(index, _)| index);
        if end > 0 {
            return Route::Static { namespace: path[..first].to_owned(), file: file[..end].replace("..", "") };
        }
    }
    let Some(method) = path[first..].strip_prefix('/') else { return Route::NoResponse };
    let end = word_end(method);
    if end == 0 {
        return Route::NoResponse;
    }
    let suffix = method[end..].strip_prefix('?').unwrap_or(&method[end..]);
    if !query_pattern(suffix) && !suffix.strip_suffix('\n').is_some_and(query_pattern) {
        return Route::NoResponse;
    }
    Route::Attribute {
        widget: path[..first].to_owned(),
        method: method[..end].to_owned(),
        parameters: query_parameters(&target),
    }
}

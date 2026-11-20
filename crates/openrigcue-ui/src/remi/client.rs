//! The browser side and wire format: the client script (from remi, Apache-2.0), the page
//! the server answers with, and the websocket text encoding (percent quoting and unquoting).

/// The client script, with its `%(…)s` fields.
const CLIENT_TEMPLATE: &str = include_str!("client.js");

/// The defaults of the pending-message queue length and the websocket timeout (ms).
const MAX_PENDING_MESSAGES: u32 = 1000;
const MESSAGING_TIMEOUT_MS: u32 = 1000;

/// The client script for the app whose identifier receives `onerror`.
pub fn client_script(app_identifier: &str) -> String {
    CLIENT_TEMPLATE
        .replace("%(host)s", "")
        .replace("%(max_pending_messages)s", &MAX_PENDING_MESSAGES.to_string())
        .replace("%(messaging_timeout)s", &MESSAGING_TIMEOUT_MS.to_string())
        .replace("%(emitter_identifier)s", app_identifier)
        .replace("%(event_name)s", "onerror")
}

/// Percent-quotes `data` as UTF-8: everything but `A-Za-z0-9_.-~/` percent-encoded.
pub fn to_websocket(data: &str) -> String {
    let mut out = String::with_capacity(data.len() * 3 / 2);
    for byte in data.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' | b'/' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Percent-unquotes `data` as UTF-8 (invalid sequences kept as they are, invalid UTF-8 replaced).
pub fn from_websocket(data: &str) -> String {
    let bytes = data.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(byte) = hex {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A websocket message from a browser.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// The text `connected`, sent once the socket opens.
    Connected,
    /// `callback/<widget identifier>/<event>/<parameters>`.
    Callback { widget: String, event: String, params: String },
    /// Anything else (ignored).
    Other,
}

/// Decodes one message: unquoted, then split at `/` into more than 3 parts whose
/// first is `callback`; the parameters are what follows the third `/`.
pub fn parse_incoming(raw: &str) -> Incoming {
    let message = from_websocket(raw);
    if message == "connected" {
        return Incoming::Connected;
    }
    let chunks: Vec<&str> = message.split('/').collect();
    if chunks.len() > 3 && chunks[0] == "callback" {
        let widget = chunks[1].to_owned();
        let event = chunks[2].to_owned();
        let start = "callback".len() + widget.len() + event.len() + 3;
        let params = message.get(start..).unwrap_or_default().to_owned();
        return Incoming::Callback { widget, event, params };
    }
    Incoming::Other
}

/// The acknowledgement, sent for every message received.
pub const MSG_ACK: &str = "3";

/// The JavaScript message: its marker followed by the code.
pub fn javascript_message(code: &str) -> String {
    format!("2{code}")
}

/// What the page's `<head>` holds (the application adds the style sheets to it).
pub struct PageHead<'a> {
    pub title: &'a str,
    pub app_identifier: &'a str,
    /// Extra `<head>` children (style sheets).
    pub extra: &'a str,
}

/// The whole page for `/` (`<!DOCTYPE html>` and the rendered page), the body holding the loading
/// container and the app's root.
pub fn page(head: &PageHead<'_>, body_attributes: &str, body_inner: &str) -> String {
    format!(
        "<!DOCTYPE html>\n<html><head>\n<meta content='text/html;charset=utf-8' http-equiv='Content-Type'>\n\
         <meta content='utf-8' http-equiv='encoding'>\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\n\
         <link rel=\"icon\" href=\"data:,\">\n{extra}\n<script>\n{script}\n</script>\n<title>{title}</title>\n</head>\
         <body {body_attributes}>{body_inner}</body></html>",
        extra = head.extra,
        script = client_script(head.app_identifier),
        title = super::escape(head.title),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A callback message, `callback/<id>/<event>/<params>`; the parameters keep their own slashes.
    #[test]
    fn incoming_messages() {
        assert_eq!(parse_incoming("connected"), Incoming::Connected);
        let raw = to_websocket("callback/123/onchange/7|value=a/b");
        assert_eq!(
            parse_incoming(&raw),
            Incoming::Callback { widget: "123".into(), event: "onchange".into(), params: "7|value=a/b".into() }
        );
        assert_eq!(
            parse_incoming(&to_websocket("callback/123/onclick/")),
            Incoming::Callback { widget: "123".into(), event: "onclick".into(), params: String::new() }
        );
        assert_eq!(parse_incoming("callback/1"), Incoming::Other);
    }

    #[test]
    fn client_script_fields() {
        let script = client_script("42");
        assert!(!script.contains("%("));
        assert!(script.contains("remi.sendCallbackParam('42','onerror',params);"));
        assert!(script.contains("this._pendingSendMessages.length < 1000"));
    }
}

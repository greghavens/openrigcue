//! The single shared client: session zero.

pub(super) const PAGE_COOKIE: &str = "remi_session=0; SameSite=Lax";

/// The parser replaces every marker in each matching semicolon-delimited token,
/// parses an integer, ignores failed tokens, and retains the last valid int.
/// Only zero can name this host's shared client; retain validity separately.
fn parse_cookie(cookie: &str) -> Option<bool> {
    let mut session = None;
    for token in cookie.split(';') {
        if !token.contains("remi_session=") {
            continue;
        }
        let text = token.replace("remi_session=", "");
        let Some(integer) = openrigcue_model::python_numeric::integer_text(&text) else { continue };
        let digits = integer.trim_start_matches(['+', '-']);
        if digits.len() > 4300 {
            continue;
        }
        session = Some(digits.bytes().all(|byte| byte == b'0'));
    }
    session
}

pub(super) fn matches_shared_client(cookie: Option<&[u8]>) -> bool {
    cookie.is_some_and(|value| {
        // HTTP header bytes are decoded as ISO-8859-1.
        let cookie: String = value.iter().copied().map(char::from).collect();
        parse_cookie(&cookie) == Some(true)
    })
}

//! Markup helpers of the interface: escaping, attributes, icons and translated text.

use crate::app::MyApp;

/// `text` escaped for HTML text and attribute values.
pub fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// The translation of `text` in the selected language (the text itself where the catalog has none).
pub fn tr(app: &MyApp, text: &str) -> String {
    app.backend.translate(text)
}

/// The translation of `text`, escaped.
pub fn t(app: &MyApp, text: &str) -> String {
    esc(&tr(app, text))
}

/// The translation of a `^name^` sentence with its values filled in, escaped.
pub fn tf(app: &MyApp, text: &str, values: &[(&str, &str)]) -> String {
    let mut out = tr(app, text);
    for (name, value) in values {
        out = out.replace(&format!("^{name}^"), value);
    }
    esc(&out)
}

/// The line icons of the interface: one stroked path each, 24 × 24.
fn icon_path(name: &str) -> &'static str {
    match name {
        "games" => "M6 8h12l3 10-4 1-3-4h-4l-3 4-4-1 3-10ZM7 10v4M5 12h4M16 11h.01M18 13h.01",
        "hardware" => "M5 4h14v12H5zM9 20h6M12 16v4M8 8h8",
        "activity" => "M18 8a6 6 0 0 0-12 0c0 7-3 7-3 9h18c0-2-3-2-3-9M10 21h4",
        "settings" => {
            "M12 8a4 4 0 1 0 0 8 4 4 0 0 0 0-8M12 2v3M12 19v3M2 12h3M19 12h3M5 5l2 2M17 17l2 2M5 19l2-2M17 7l2-2"
        }
        "motion" => "M3 12h18M12 3v18M8 7l4-4 4 4M8 17l4 4 4-4",
        "wind" => "M3 8h12a3 3 0 1 0-3-3M3 12h16a3 3 0 1 1-3 3M3 16h5",
        "shake" => "M3 10v4M7 6v12M11 3v18M15 7v10M19 5v14M23 10v4",
        "check" => "m5 12 4 4 10-10",
        "warning" => "M12 3 2 21h20L12 3ZM12 9v5M12 17h.01",
        "search" => "M11 4a7 7 0 1 0 0 14 7 7 0 0 0 0-14M21 21l-5-5",
        "compensation" => "M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18M12 7v5l3 3",
        "belt" => "M4 4l8 8 8-8M12 12v9",
        "flag" => "M5 21V4h11l-2 4 2 4H5",
        "led" => "M9 18h6M10 21h4M12 3a6 6 0 0 0-4 10c1 1 1 2 1 3h6c0-1 0-2 1-3a6 6 0 0 0-4-10",
        "hud" => "M3 6h18v10H3zM8 20h8",
        _ => "M4 7h16M4 17h16M8 4v6M16 14v6",
    }
}

/// An inline line icon.
pub fn icon(name: &str) -> String {
    format!("<svg class=\"icon\" viewBox=\"0 0 24 24\" aria-hidden=\"true\"><path d=\"{}\"/></svg>", icon_path(name))
}

/// A number as the controls show it: whole numbers without a fraction.
pub fn num(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else if value.is_finite() {
        let text = format!("{value:.2}");
        text.trim_end_matches('0').trim_end_matches('.').to_owned()
    } else {
        "—".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_covers_markup_and_quotes() {
        assert_eq!(esc("a<b>&\"c'"), "a&lt;b&gt;&amp;&quot;c&#39;");
    }

    #[test]
    fn numbers_drop_needless_fractions() {
        assert_eq!(num(35.0), "35");
        assert_eq!(num(1.25), "1.25");
        assert_eq!(num(0.5), "0.5");
        assert_eq!(num(f64::NAN), "—");
    }
}

//! OpenRigCue's visual resources, compiled into the binary, and the browser page head.

use axum::http::header;
use axum::response::{IntoResponse, Response};

include!(concat!(env!("OUT_DIR"), "/res_files.rs"));
mod python_mime_types;

/// Product image and theme namespace; hardware and protocol identities remain separate.
pub const COMPANY_ID: &str = "openrigcue";

/// The content type of a file name, from the standard suffix table plus the font types.
pub fn content_type(path: &str) -> &'static str {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let Some((base, extension)) = split_extension(name) else { return "application/octet-stream" };
    let mut lower = extension.to_lowercase();
    match lower.as_str() {
        ".svgz" => return mime_for_extension(".svg"),
        ".tgz" | ".taz" | ".tz" | ".tbz2" | ".txz" => return mime_for_extension(".tar"),
        _ => {}
    }
    // Encoding suffixes are case sensitive and removed only once, after alias expansion.
    if matches!(extension, ".gz" | ".Z" | ".bz2" | ".xz" | ".br") {
        lower = split_extension(base).map_or("", |(_, extension)| extension).to_lowercase();
    }
    mime_for_extension(&lower)
}

fn split_extension(name: &str) -> Option<(&str, &str)> {
    let index = name.rfind('.')?;
    if name[..index].bytes().all(|byte| byte == b'.') {
        return None;
    }
    Some((&name[..index], &name[index..]))
}

fn mime_for_extension(extension: &str) -> &'static str {
    match extension {
        // The embedded fonts' types; the suffix table has no font entries.
        ".ttf" => "font/ttf",
        ".woff" => "font/woff",
        ".eot" => "application/vnd.ms-fontobject",
        _ => python_mime_types::TYPES
            .binary_search_by_key(&extension, |(extension, _)| *extension)
            .map_or("application/octet-stream", |index| python_mime_types::TYPES[index].1),
    }
}

/// The file at `path` below `res/`.
pub fn file(path: &str) -> Option<&'static [u8]> {
    RES.iter().find(|(name, _)| *name == path).map(|(_, body)| *body)
}

/// The headers of a known namespace are sent before the file is opened. An unavailable
/// file therefore retains200 and its MIME header with an empty body.
pub fn serve(path: &str) -> Response {
    let body = file(path).unwrap_or_else(|| {
        tracing::error!(path, "error processing GET request");
        &[]
    });
    ([(header::CONTENT_TYPE, content_type(path)), (header::CACHE_CONTROL, "public, max-age=86400")], body)
        .into_response()
}

/// The constant parts of the icon links in the page head, joined by the company id.
const MY_CSS_HEAD: [&str; 31] = [
    "<link rel=\"icon\" type=\"image/svg+xml\" href=\"/res:images/openrigcue/mark.svg\">\n                <link rel=\"icon\" type=\"image/png\" sizes=\"144x144\" href=\"/res:images/",
    "/iconsizes/android144.png\">\n                <link rel=\"icon\" type=\"image/png\" sizes=\"192x192\" href=\"/res:images/",
    "/iconsizes/android192.png\">\n                <!-- <link rel=\"icon\" type=\"image/png\" sizes=\"36x36\" href=\"/res:images/",
    "/iconsizes/android36.png\"> -->\n                <link rel=\"icon\" type=\"image/png\" sizes=\"48x48\" href=\"/res:images/",
    "/iconsizes/android48.png\">\n                <link rel=\"icon\" type=\"image/png\" sizes=\"512x512\" href=\"/res:images/",
    "/iconsizes/android512.png\">\n                <link rel=\"icon\" type=\"image/png\" sizes=\"72x72\" href=\"/res:images/",
    "/iconsizes/android72.png\">\n                <link rel=\"icon\" type=\"image/png\" sizes=\"96x96\" href=\"/res:images/",
    "/iconsizes/android96.png\">\n                <!-- <link rel=\"icon\" type=\"image/png\" sizes=\"16x16\" href=\"/res:images/",
    "/iconsizes/favicon16.png\"> -->\n                <link rel=\"apple-touch-icon\" sizes=\"1024x1024\" href=\"/res:images/",
    "/iconsizes/ios1024.png\">\n                <link rel=\"apple-touch-icon\" sizes=\"120x120\" href=\"/res:images/",
    "/iconsizes/ios120.png\">\n                <link rel=\"apple-touch-icon\" sizes=\"152x152\" href=\"/res:images/",
    "/iconsizes/ios152.png\">\n                <link rel=\"apple-touch-icon\" sizes=\"167x167\" href=\"/res:images/",
    "/iconsizes/ios167.png\">\n                <link rel=\"apple-touch-icon\" sizes=\"180x180\" href=\"/res:images/",
    "/iconsizes/ios180.png\">\n                <link rel=\"apple-touch-icon\" sizes=\"22x22\" href=\"/res:images/",
    "/iconsizes/ios22.png\">\n                <link rel=\"apple-touch-icon\" sizes=\"29x29\" href=\"/res:images/",
    "/iconsizes/ios29.png\">\n                <link rel=\"apple-touch-icon\" sizes=\"44x44\" href=\"/res:images/",
    "/iconsizes/ios44.png\">\n                <link rel=\"apple-touch-icon\" sizes=\"50x50\" href=\"/res:images/",
    "/iconsizes/ios50.png\">\n                <link rel=\"apple-touch-icon\" sizes=\"58x58\" href=\"/res:images/",
    "/iconsizes/ios58.png\">\n                <link rel=\"apple-touch-icon\" sizes=\"60x60\" href=\"/res:images/",
    "/iconsizes/ios60.png\">\n                <link rel=\"apple-touch-icon\" sizes=\"66x66\" href=\"/res:images/",
    "/iconsizes/ios66.png\">\n                <link rel=\"apple-touch-icon\" sizes=\"76x76\" href=\"/res:images/",
    "/iconsizes/ios76.png\">\n                <link rel=\"apple-touch-icon\" sizes=\"80x80\" href=\"/res:images/",
    "/iconsizes/ios80.png\">\n                <!-- <link rel=\"icon\" type=\"image/png\" sizes=\"22x22\" href=\"/res:images/",
    "/iconsizes/systray22.png\"> -->\n                <!-- <link rel=\"icon\" type=\"image/png\" sizes=\"16x16\" href=\"/res:images/",
    "/iconsizes/windows16.png\"> -->\n                <link rel=\"icon\" type=\"image/png\" sizes=\"256x256\" href=\"/res:images/",
    "/iconsizes/windows256.png\">\n                <link rel=\"icon\" type=\"image/png\" sizes=\"48x48\" href=\"/res:images/",
    "/iconsizes/windows48.png\">\n                <link rel=\"icon\" type=\"image/png\" sizes=\"96x96\" href=\"/res:images/",
    "/iconsizes/windows96.png\">\n                <link rel=\"manifest\" href=\"/res:images/",
    "/iconsizes/manifest.json\">\n                <meta name=\"msapplication-TileColor\" content=\"#000000\">\n                <meta name=\"msapplication-TileImage\" content=\"/res:images/",
    "/iconsizes/windows256.png\">\n                <meta name=\"msapplication-config\" content=\"/res:images/",
    "/iconsizes/browserconfig.xml\" />\n            ",
];

/// The interface's style sheet (every theme) and its browser script, below `res/`.
pub const UI_STYLESHEET: &str = "css/openrigcue-ui.css";
pub const UI_SCRIPT: &str = "js/openrigcue-ui.js";

/// What the application adds to the page head, in order: the icon links, the interface's style sheet and its
/// script.
pub fn head() -> String {
    let mycss = MY_CSS_HEAD.join(COMPANY_ID);
    format!("{mycss}<link href='/res:{UI_STYLESHEET}' rel='stylesheet' />\n<script src=\"/res:{UI_SCRIPT}\"></script>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn res_folder_is_embedded() {
        assert!(file(UI_STYLESHEET).is_some());
        assert!(file(UI_SCRIPT).is_some());
        assert!(file("fonts/dmsans.ttf").is_some() && file("fonts/barlowcondensed.ttf").is_some());
        assert!(file("images/openrigcue/mark.svg").is_some() && file("images/openrigcue/6dof.png").is_some());
        let splash = file("images/openrigcue/splash.png").unwrap();
        assert_eq!(&splash[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(u32::from_be_bytes(splash[16..20].try_into().unwrap()), 960);
        assert_eq!(u32::from_be_bytes(splash[20..24].try_into().unwrap()), 540);
        let head = head();
        assert!(head.contains("/res:images/openrigcue/iconsizes/android144.png"));
        assert!(head.contains("<link href='/res:css/openrigcue-ui.css' rel='stylesheet' />"));
        assert!(head.ends_with("<script src=\"/res:js/openrigcue-ui.js\"></script>"));
        assert!(!head.contains("style-openrigcue.css") && !head.contains("theme-"), "only the interface's style sheet");
        assert!(!head.contains("{"));
    }

    #[test]
    fn authored_visual_assets_resolve_without_original_product_resources() {
        for (path, _) in RES {
            assert!(!path.starts_with("source/"), "editable artwork embedded: {path}");
            assert!(!path.ends_with("-bg.svg"), "ornamental background embedded: {path}");
            assert!(!path.contains("/srs/") && !path.contains("srs-fonts"), "copied resource: {path}");
            assert!(!path.ends_with(".ico") && !path.ends_with(".jpeg"), "old product artwork: {path}");
        }
        // Only the interface's own style sheet and script are shipped.
        let sheets: Vec<&str> = RES.iter().map(|(path, _)| *path).filter(|path| path.ends_with(".css")).collect();
        assert_eq!(sheets, [UI_STYLESHEET]);
        let scripts: Vec<&str> = RES.iter().map(|(path, _)| *path).filter(|path| path.ends_with(".js")).collect();
        assert_eq!(scripts, [UI_SCRIPT]);
        for path in [UI_STYLESHEET] {
            let css = std::str::from_utf8(file(path).unwrap()).unwrap();
            for reference in css.split("url(").skip(1) {
                let url = reference.split(')').next().unwrap().trim_matches(['\'', '"', ' ']);
                if let Some(asset) = url.strip_prefix("/res:") {
                    assert!(file(asset).is_some(), "{path} references missing {asset}");
                }
            }
        }
        for reference in head().split("/res:").skip(1) {
            let path = reference.split(['\'', '"', '>']).next().unwrap();
            assert!(file(path).is_some(), "head references missing {path}");
        }
        let manifest: serde_json::Value =
            serde_json::from_slice(file("images/openrigcue/iconsizes/manifest.json").unwrap()).unwrap();
        assert_eq!(manifest["name"], "OpenRigCue");
        for icon in manifest["icons"].as_array().unwrap() {
            let path = icon["src"].as_str().unwrap().strip_prefix("/res:").unwrap();
            let png = file(path).unwrap();
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
            let width = u32::from_be_bytes(png[16..20].try_into().unwrap());
            let height = u32::from_be_bytes(png[20..24].try_into().unwrap());
            assert_eq!(icon["sizes"], format!("{width}x{height}"));
        }
    }

    #[tokio::test]
    async fn authored_images_are_served_with_their_actual_bytes_and_mime() {
        use http_body_util::BodyExt;

        for (path, mime) in [
            ("images/openrigcue/splash.png", "image/png"),
            ("images/openrigcue/mark.svg", "image/svg+xml"),
            ("images/openrigcue/6dof.png", "image/png"),
        ] {
            let response = serve(path);
            assert_eq!(response.headers()[header::CONTENT_TYPE], mime);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(body.as_ref(), file(path).unwrap());
            if mime == "image/svg+xml" {
                assert!(std::str::from_utf8(&body).unwrap().contains("<svg"));
            } else {
                assert!(body.starts_with(b"\x89PNG\r\n\x1a\n"));
            }
        }
    }
}

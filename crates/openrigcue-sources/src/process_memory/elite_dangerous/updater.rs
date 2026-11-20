//! The offsets update check of the Elite Dangerous listener: every 8 hours, find the newest offsets
//! archive attached to the plugin's forum thread, download it when it changed, and load it.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use super::offsets::{
    newest_attachment, parse_offsets_xml_with, xml_from_archive, ArchiveError, OffsetChange, Offsets,
};
use crate::process_memory::http::{HttpGet, HttpReply};
use crate::process_memory::python::{int_from_str, is_space};

/// The plugin thread; `page-999` redirects to its last page.
pub const THREAD_URL: &str =
    "https://www.xsimulator.net/community/threads/elite-dangerous-horizons-odyssey-plugin-xml-edition.12955/page-";
/// Time between update checks (8 h) and the request timeout (20 s).
pub const CHECK_PERIOD: Duration = Duration::from_secs(28_800);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Headers sent with every request.
const HEADERS: [(&str, &str); 1] = [("User-Agent", "Mozilla/5.0")];

/// Where the archive and the URL of its last download are kept (the documents folder).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OffsetFiles {
    /// `EliteDangerous64_Offsets.zip`.
    pub archive: PathBuf,
    /// `latest_ed_offsets.txt`.
    pub last_url: PathBuf,
}

impl OffsetFiles {
    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        OffsetFiles { archive: dir.join("EliteDangerous64_Offsets.zip"), last_url: dir.join("latest_ed_offsets.txt") }
    }
}

/// What ends the update thread (an error nothing handles).
#[derive(Debug, thiserror::Error)]
pub enum UpdateStopped {
    #[error("last page number unreadable in {0}")]
    PageNumber(String),
    #[error("offsets download failed: {0}")]
    Download(String),
    #[error("offsets files: {0}")]
    Files(#[from] io::Error),
    #[error(transparent)]
    Archive(#[from] ArchiveError),
}

/// The result of one check that did not stop the thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// Offsets loaded from the (possibly freshly downloaded) archive; empty when the XML was unusable.
    Loaded(Offsets),
    /// The thread or none of its pages could be read; offsets unchanged.
    NothingFound,
}

/// The last page of the thread: the number at the end of the URL `page-999` redirects to. A transport failure or a
/// status other than 200 yields `None`; an unparsable number stops the thread.
fn last_page(http: &mut dyn HttpGet) -> Result<Option<i64>, UpdateStopped> {
    let reply = match http.get(&format!("{THREAD_URL}999"), &HEADERS) {
        Ok(reply) if reply.status == 200 => reply,
        Ok(_) => {
            tracing::error!("Failed to retrieve the last page.");
            return Ok(None);
        }
        Err(error) => {
            tracing::error!(%error, "Failed to retrieve the last page");
            return Ok(None);
        }
    };
    let number = reply.url.rsplit('-').next().unwrap_or_default();
    int_from_str(number).map(Some).ok_or(UpdateStopped::PageNumber(reply.url))
}

/// The newest offsets attachment linked from one page.
fn newest_on_page(http: &mut dyn HttpGet, page: i64) -> Option<String> {
    match http.get(&format!("{THREAD_URL}{page}"), &HEADERS) {
        Ok(HttpReply { status: 200, body, .. }) => newest_attachment(&String::from_utf8_lossy(&body)),
        Ok(_) => {
            tracing::warn!(page, "Failed to fetch page");
            None
        }
        Err(error) => {
            tracing::error!(page, %error, "Failed to fetch page");
            None
        }
    }
}

/// The remembered URL, stripped; `None` when the file does not exist.
fn stored_url(files: &OffsetFiles) -> Result<Option<String>, UpdateStopped> {
    match fs::read_to_string(&files.last_url) {
        Ok(text) => Ok(Some(text.trim_matches(is_space).to_owned())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Downloads the archive: GET, fail on an error status, write the body to the archive file.
fn download(http: &mut dyn HttpGet, url: &str, files: &OffsetFiles) -> Result<(), UpdateStopped> {
    let reply = http.get(url, &HEADERS).map_err(|e| UpdateStopped::Download(e.to_string()))?;
    if reply.status >= 400 {
        return Err(UpdateStopped::Download(format!("{url}: HTTP {}", reply.status)));
    }
    fs::write(&files.archive, &reply.body)?;
    Ok(())
}

/// One update check: from the last page down to page 1, the first page with an offsets attachment
/// decides; a new URL is remembered and downloaded; the archive is then loaded.
pub fn check_for_update(http: &mut dyn HttpGet, files: &OffsetFiles) -> Result<CheckOutcome, UpdateStopped> {
    check_for_update_with(http, files, |_| {})
}

pub(super) fn check_for_update_with(
    http: &mut dyn HttpGet,
    files: &OffsetFiles,
    mut publish: impl FnMut(OffsetChange),
) -> Result<CheckOutcome, UpdateStopped> {
    let Some(last) = last_page(http)? else { return Ok(CheckOutcome::NothingFound) };
    let stored = stored_url(files)?;
    for page in (1..=last).rev() {
        let Some(latest) = newest_on_page(http, page) else { continue };
        if stored.as_deref() != Some(latest.as_str()) {
            fs::write(&files.last_url, &latest)?;
            download(http, &latest, files)?;
        }
        let archive = fs::read(&files.archive)?;
        let offsets = match xml_from_archive(&archive)? {
            Some(xml) => parse_offsets_xml_with(&xml, &mut publish),
            None => {
                tracing::error!("No XML file found in the ZIP archive.");
                crate::printed::line("failed to load ed offsets");
                publish(OffsetChange::Reset);
                Offsets::default()
            }
        };
        return Ok(CheckOutcome::Loaded(offsets));
    }
    tracing::error!("No file found after checking all pages.");
    Ok(CheckOutcome::NothingFound)
}

#[cfg(test)]
mod tests {
    use super::super::offsets::parse_offsets_xml;
    use super::*;
    use crate::process_memory::http::testing::ScriptedHttp;

    fn archive(xml: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut writer = zip::ZipWriter::new(io::Cursor::new(&mut bytes));
            writer.start_file("offsets.xml", zip::write::SimpleFileOptions::default()).unwrap();
            io::Write::write_all(&mut writer, xml.as_bytes()).unwrap();
            writer.finish().unwrap();
        }
        bytes
    }

    fn temp_files(name: &str) -> OffsetFiles {
        let dir = std::env::temp_dir().join(format!("openrigcue-ed-{name}-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        OffsetFiles::in_dir(dir)
    }

    const LINK: &str = "https://www.xsimulator.net/community/attachments/elitedangerous64_offsets-zip.77/";

    /// The last page comes from the redirect, pages walked downwards, new URL stored and downloaded.
    #[test]
    fn finds_downloads_and_loads() {
        let files = temp_files("full");
        let _ = fs::remove_file(&files.last_url);
        let http = ScriptedHttp::default();
        let last = format!("{THREAD_URL}999");
        http.replies
            .lock()
            .unwrap()
            .push_back((last.clone(), Ok(HttpReply { status: 200, url: format!("{THREAD_URL}3"), body: Vec::new() })));
        http.reply(&format!("{THREAD_URL}3"), 200, "no attachment here");
        http.reply(&format!("{THREAD_URL}2"), 200, "see attachments/elitedangerous64_offsets-zip.77/ now");
        let zip = archive(r#"<Root><Odyssey><Yaw Offsets="1" Offset_0="10"/></Odyssey></Root>"#);
        http.replies
            .lock()
            .unwrap()
            .push_back((LINK.into(), Ok(HttpReply { status: 200, url: LINK.into(), body: zip })));
        let outcome = check_for_update(&mut http.clone(), &files).unwrap();
        let CheckOutcome::Loaded(offsets) = outcome else { panic!("not loaded") };
        assert_eq!(offsets.chains(super::super::offsets::EdBuild::Odyssey).unwrap().len(), 1);
        assert_eq!(fs::read_to_string(&files.last_url).unwrap(), LINK);
        assert_eq!(http.requested.lock().unwrap().len(), 4);
    }

    /// An unchanged URL is not downloaded again, the stored archive is reloaded.
    #[test]
    fn unchanged_url_reloads_without_download() {
        let files = temp_files("same");
        fs::write(&files.last_url, format!("{LINK}\n")).unwrap();
        fs::write(&files.archive, archive("<Root/>")).unwrap();
        let http = ScriptedHttp::default();
        http.replies.lock().unwrap().push_back((
            format!("{THREAD_URL}999"),
            Ok(HttpReply { status: 200, url: format!("{THREAD_URL}1"), body: Vec::new() }),
        ));
        http.reply(&format!("{THREAD_URL}1"), 200, "attachments/elitedangerous64_offsets-zip.77/");
        assert!(matches!(check_for_update(&mut http.clone(), &files).unwrap(), CheckOutcome::Loaded(_)));
        assert!(!http.requested.lock().unwrap().iter().any(|u| u == LINK));
    }

    /// Last-page failures: a transport error finds nothing; a URL without a page number stops the thread.
    #[test]
    fn last_page_failures() {
        let files = temp_files("fail");
        let http = ScriptedHttp::default();
        http.fail(&format!("{THREAD_URL}999"));
        assert_eq!(check_for_update(&mut http.clone(), &files).unwrap(), CheckOutcome::NothingFound);
        http.replies.lock().unwrap().push_back((
            format!("{THREAD_URL}999"),
            Ok(HttpReply {
                status: 200,
                url: "https://www.xsimulator.net/community/threads/x.12955/".into(),
                body: Vec::new(),
            }),
        ));
        assert!(matches!(check_for_update(&mut http.clone(), &files), Err(UpdateStopped::PageNumber(_))));
    }

    fn available_archive(bytes: Vec<u8>) -> ScriptedHttp {
        let http = ScriptedHttp::default();
        http.replies.lock().unwrap().push_back((
            format!("{THREAD_URL}999"),
            Ok(HttpReply { status: 200, url: format!("{THREAD_URL}1"), body: Vec::new() }),
        ));
        http.reply(&format!("{THREAD_URL}1"), 200, "attachments/elitedangerous64_offsets-zip.77/");
        http.replies
            .lock()
            .unwrap()
            .push_back((LINK.into(), Ok(HttpReply { status: 200, url: LINK.into(), body: bytes })));
        http
    }

    #[test]
    fn native_update_publishes_each_completed_build_then_clears_on_semantic_error() {
        use super::super::offsets::{EdBuild, Force};
        for bad_second in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let files = OffsetFiles::in_dir(dir.path());
            let mut retained = parse_offsets_xml(
                r#"<Root><Horizons><Speed Offsets="1" Offset_0="1"/></Horizons><Odyssey><Yaw Offsets="1" Offset_0="2"/></Odyssey></Root>"#,
            );
            let xml = if bad_second {
                r#"<Root><Horizons><Speed Offsets="1" Offset_0="A"/></Horizons><Odyssey><Invalid Offsets="0"/></Odyssey></Root>"#
            } else {
                r#"<Root><Horizons><Speed Offsets="1" Offset_0="A"/></Horizons><Odyssey><Yaw Offsets="1" Offset_0="B"/></Odyssey></Root>"#
            };
            let http = available_archive(archive(xml));
            let mut visible = Vec::new();
            let result = check_for_update_with(&mut http.clone(), &files, |change| {
                retained.apply(change);
                visible.push(retained.clone());
            })
            .unwrap();
            assert_eq!(visible.len(), 2);
            assert_eq!(visible[0].chains(EdBuild::Horizons), Some([(Force::Speed, vec![10])].as_slice()));
            assert_eq!(visible[0].chains(EdBuild::Odyssey), Some([(Force::Yaw, vec![2])].as_slice()));
            if bad_second {
                assert!(visible[1].is_empty());
            } else {
                assert_eq!(visible[1].chains(EdBuild::Odyssey), Some([(Force::Yaw, vec![11])].as_slice()));
            }
            assert_eq!(result, CheckOutcome::Loaded(retained));
        }
    }

    #[test]
    fn native_update_bad_archive_publishes_nothing_and_absent_xml_resets() {
        for invalid_xml in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let files = OffsetFiles::in_dir(dir.path());
            let bytes = if invalid_xml {
                archive("<Root><Odyssey>")
            } else {
                let mut bytes = Vec::new();
                let mut zip = zip::ZipWriter::new(io::Cursor::new(&mut bytes));
                zip.start_file("notes.txt", zip::write::SimpleFileOptions::default()).unwrap();
                io::Write::write_all(&mut zip, b"no XML").unwrap();
                zip.finish().unwrap();
                bytes
            };
            let http = available_archive(bytes);
            let mut changes = Vec::new();
            let result = check_for_update_with(&mut http.clone(), &files, |change| changes.push(change));
            if invalid_xml {
                assert!(result.is_err());
                assert!(changes.is_empty(), "XML syntax error escapes before load_pointers");
            } else {
                assert!(matches!(result, Ok(CheckOutcome::Loaded(offsets)) if offsets.is_empty()));
                assert!(matches!(changes.as_slice(), [OffsetChange::Reset]));
            }
        }
    }

    #[test]
    fn native_archive_syntax_failure_precedes_pointer_conversion() {
        let dir = tempfile::tempdir().unwrap();
        let files = OffsetFiles::in_dir(dir.path());
        let prior = parse_offsets_xml(r#"<Root><Odyssey><Yaw Offsets="1" Offset_0="10"/></Odyssey></Root>"#);
        let mut retained = prior.clone();
        fs::write(&files.last_url, "previous URL").unwrap();
        let bytes = archive("<Root><Odyssey>");
        let http = available_archive(bytes.clone());
        // The actual update worker only assigns loaded offsets; any error ends it.
        let result = check_for_update(&mut http.clone(), &files);
        if let Ok(CheckOutcome::Loaded(offsets)) = &result {
            retained = offsets.clone();
        }
        assert!(result.is_err(), "ET.parse syntax failure escapes load_zip_file");
        assert_eq!(retained, prior, "load_pointers was never entered");
        assert_eq!(fs::read_to_string(&files.last_url).unwrap(), LINK, "URL stored before download");
        assert_eq!(fs::read(&files.archive).unwrap(), bytes, "download completed before XML parse");
        assert_eq!(http.requested.lock().unwrap().len(), 3);
    }

    #[test]
    fn native_cached_archive_error_does_not_redownload_or_become_empty_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let files = OffsetFiles::in_dir(dir.path());
        fs::write(&files.last_url, LINK).unwrap();
        let malformed = archive("<Root><Odyssey></Root>");
        fs::write(&files.archive, &malformed).unwrap();
        let http = available_archive(archive("<Root/>"));
        assert!(check_for_update(&mut http.clone(), &files).is_err());
        assert_eq!(http.requested.lock().unwrap().len(), 2);
        assert_eq!(fs::read(&files.archive).unwrap(), malformed);
    }

    #[test]
    fn native_archive_semantic_errors_and_absent_xml_keep_their_caught_result() {
        for bytes in [
            archive(r#"<Root><Odyssey><Yaw Offsets="1"/></Odyssey></Root>"#),
            archive(r#"<Root><Odyssey><Unknown Offsets="0"/></Odyssey></Root>"#),
            {
                let mut bytes = Vec::new();
                let mut zip = zip::ZipWriter::new(io::Cursor::new(&mut bytes));
                zip.start_file("notes.txt", zip::write::SimpleFileOptions::default()).unwrap();
                io::Write::write_all(&mut zip, b"no XML").unwrap();
                zip.finish().unwrap();
                bytes
            },
        ] {
            let dir = tempfile::tempdir().unwrap();
            let files = OffsetFiles::in_dir(dir.path());
            let http = available_archive(bytes);
            let CheckOutcome::Loaded(offsets) = check_for_update(&mut http.clone(), &files).unwrap() else {
                panic!("semantic errors are caught by load_pointers");
            };
            assert!(offsets.is_empty());
        }
    }
}

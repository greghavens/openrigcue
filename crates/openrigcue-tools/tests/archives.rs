use openrigcue_tools::{firmware, usb};
use std::{fs, io::Write};
#[test]
fn archive_import_selects_firmware_and_never_overwrites() {
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        for (name, bytes) in [
            ("payload/image.bin", b"firmware".as_slice()),
            ("payload/updater.exe", b"never run".as_slice()),
            ("payload/load.bat", b"never run".as_slice()),
        ] {
            zip.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(firmware::import_archive(cursor.get_ref().clone(), tmp.path()).unwrap().len(), 1);
    assert!(!tmp.path().join("payload/updater.exe").exists());
    firmware::import_archive(cursor.get_ref().clone(), tmp.path()).unwrap();
    fs::write(tmp.path().join("payload/image.bin"), "existing").unwrap();
    assert!(firmware::import_archive(cursor.into_inner(), tmp.path()).is_err());
}
#[test]
fn usb_xml_snapshot_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("1-2");
    fs::create_dir(&p).unwrap();
    fs::write(p.join("idVendor"), "0483").unwrap();
    fs::write(p.join("idProduct"), "5740").unwrap();
    fs::write(p.join("product"), "A <controller> & motor").unwrap();
    let devices = usb::enumerate(tmp.path()).unwrap();
    let text = usb::xml_report(&devices);
    let result = usb::import_report(&text).unwrap();
    assert_eq!(result[0].name, "A <controller> & motor");
    assert_eq!(result[0].vendor, 0x483);
}

#[test]
fn malformed_hex_returns_an_error_instead_of_panicking() {
    assert!(firmware::decode_hex(":0é00\n:00000001FF").is_err());
    assert!(firmware::decode_hex(":0400000001020304F2\n:0400000001020305F1\n:00000001FF").is_err());
}

#[test]
fn zip_path_traversal_is_rejected_before_writing() {
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        zip.start_file("../escape.bin", zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(b"firmware").unwrap();
        zip.finish().unwrap();
    }
    let temp = tempfile::tempdir().unwrap();
    let destination = temp.path().join("firmware");
    assert!(firmware::import_archive(cursor.into_inner(), &destination).is_err());
    assert!(!temp.path().join("escape.bin").exists());
}

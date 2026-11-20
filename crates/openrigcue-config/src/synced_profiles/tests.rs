use super::*;
use std::io::Write;
fn fixture() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("profiles")).unwrap();
    let target = Target { game: "ac".into(), page: Category::Motion, vehicle: Vehicle::Game(0) };
    let id = hash(&serde_json::to_vec(&(&target, "resource^author")).unwrap());
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file("tune.json", zip::write::SimpleFileOptions::default()).unwrap();
    zip.write_all(br#"{"game":"ac","vehicle":0,"srs_version":"2026.6.14.2","motion_max_power":75}"#).unwrap();
    let data = zip.finish().unwrap().into_inner();
    let sha = hash(&data);
    let path = format!("profiles/{id}-{sha}.srs");
    fs::write(dir.path().join(&path), &data).unwrap();
    let manifest = serde_json::json!({"format":1,"targets":[target],"records":{id.clone():{"target":target,"resource_id":"resource^author","remote_version":2,"alias":"Test Driver","file":path,"sha256":sha,"bytes":data.len()}}});
    fs::write(dir.path().join("manifest.json"), serde_json::to_vec(&manifest).unwrap()).unwrap();
    (dir, id)
}
#[test]
fn local_library_reads_synced_schema_and_verifies_again_before_load() {
    let (dir, id) = fixture();
    let entries = list(dir.path()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].state, FileState::Ready);
    assert_eq!(entries[0].author, "Test Driver");
    // A manifest written before descriptions were stored still loads, without one.
    assert_eq!(entries[0].description, "");
    let path = dir.path().join("manifest.json");
    let mut data: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    data["records"][&id]["description"] = "DOF REALITY [H3]".into();
    fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
    assert_eq!(list(dir.path()).unwrap()[0].description, "DOF REALITY [H3]");
    assert_eq!(load(dir.path(), &id).unwrap().0.description, "DOF REALITY [H3]");
    let (_, content) = load(dir.path(), &id).unwrap();
    assert_eq!(content.item("motion_max_power").unwrap(), &75.into());
    let manifest = manifest(dir.path()).unwrap().unwrap();
    let file = &manifest.records[&id].file;
    fs::write(dir.path().join(file), b"local edit").unwrap();
    assert_eq!(list(dir.path()).unwrap()[0].state, FileState::Edited);
    assert!(load(dir.path(), &id).is_err());
    fs::remove_file(dir.path().join(file)).unwrap();
    assert_eq!(list(dir.path()).unwrap()[0].state, FileState::Missing);
}
#[test]
fn local_library_handles_empty_and_rejects_manifest_traversal() {
    let empty = tempfile::tempdir().unwrap();
    assert!(list(empty.path()).unwrap().is_empty());
    let (dir, id) = fixture();
    let path = dir.path().join("manifest.json");
    let mut data: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    data["records"][&id]["file"] = "../escape.srs".into();
    fs::write(path, serde_json::to_vec(&data).unwrap()).unwrap();
    assert!(list(dir.path()).is_err());
}

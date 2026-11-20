//! The 7-Zip Deflate port against 7-Zip itself.
//!
//! The streams of the mod zips are pinned in `tests/package.rs` (compressed
//! sizes, and size and CRC32 of each built zip).  Here the port is compared
//! with the streams `7z a -tzip -mx=5` writes for inputs that exercise the other
//! paths of the encoder: several blocks (the value limit and the 60923-byte
//! block size), stored blocks for incompressible data, long matches past the
//! 32 fast bytes, matches at distance 32768, tiny inputs, and length-limited
//! Huffman tables.  The test is skipped when no `7z` is on `PATH`.

use std::path::Path;
use std::process::Command;

use openrigcue_fs_mods::deflate7z::deflate;

fn u16_at(b: &[u8], i: usize) -> usize {
    u16::from_le_bytes([b[i], b[i + 1]]) as usize
}

fn u32_at(b: &[u8], i: usize) -> usize {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize
}

fn run_7z(args: &[&std::ffi::OsStr]) -> Option<()> {
    let status = Command::new("7z").args(args).arg("-bso0").arg("-bsp0").status().ok()?;
    assert!(status.success(), "7z failed");
    Some(())
}

/// Deflate stream of the only entry of a 7-Zip zip, or `None` when 7-Zip
/// fell back to storing the file (method 0, for data that does not shrink).
fn seven_zip_zip_stream(dir: &Path, name: &str, data: &[u8]) -> Option<Vec<u8>> {
    let src = dir.join(name);
    std::fs::write(&src, data).unwrap();
    let zip = dir.join(format!("{name}.zip"));
    let _ = std::fs::remove_file(&zip);
    run_7z(&["a".as_ref(), "-tzip".as_ref(), "-mx=5".as_ref(), zip.as_os_str(), src.as_os_str()]).expect("7z runs");
    let z = std::fs::read(&zip).unwrap();
    assert_eq!(u32_at(&z, 0), 0x0403_4b50);
    if u16_at(&z, 8) != 8 {
        return None;
    }
    let csize = u32_at(&z, 18);
    let start = 30 + u16_at(&z, 26) + u16_at(&z, 28);
    Some(z[start..start + csize].to_vec())
}

/// Deflate stream of a 7-Zip gzip file (same encoder and level-5
/// properties; gzip never falls back to storing the file).
fn seven_zip_gzip_stream(dir: &Path, name: &str, data: &[u8]) -> Vec<u8> {
    let src = dir.join(name);
    std::fs::write(&src, data).unwrap();
    let gz = dir.join(format!("{name}.gz"));
    let _ = std::fs::remove_file(&gz);
    run_7z(&["a".as_ref(), "-tgzip".as_ref(), "-mx=5".as_ref(), gz.as_os_str(), src.as_os_str()]).expect("7z runs");
    let g = std::fs::read(&gz).unwrap();
    assert_eq!(&g[..3], &[0x1f, 0x8b, 8]);
    let flags = g[3];
    let mut p = 10;
    if flags & 4 != 0 {
        p += 2 + u16_at(&g, p);
    }
    if flags & 8 != 0 {
        p += g[p..].iter().position(|&c| c == 0).unwrap() + 1;
    }
    if flags & 16 != 0 {
        p += g[p..].iter().position(|&c| c == 0).unwrap() + 1;
    }
    if flags & 2 != 0 {
        p += 2;
    }
    g[p..g.len() - 8].to_vec()
}

fn inputs() -> Vec<(&'static str, Vec<u8>)> {
    let mut seed = 0x9e37_79b9u32;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    let words: Vec<&[u8]> = vec![b"local ", b"function ", b"TelemetryMod", b".Context", b" = ", b"nil", b"\r\n", b"end", b"(", b")", b"\"", b"0.0000, "];
    let mut text = Vec::new();
    while text.len() < 300_000 {
        text.extend_from_slice(words[rnd() as usize % words.len()]);
    }
    let random: Vec<u8> = (0..150_000).map(|_| rnd() as u8).collect();
    let mut mixed = Vec::new();
    for i in 0..40 {
        if i % 3 == 0 {
            mixed.extend((0..5000).map(|_| rnd() as u8));
        } else {
            mixed.extend_from_slice(&text[i * 1000..i * 1000 + 7000]);
        }
    }
    let mut far = (0..32_768).map(|_| rnd() as u8).collect::<Vec<u8>>();
    let head = far[..2000].to_vec();
    far.extend_from_slice(&head);
    let skewed: Vec<u8> = (0..120_000).map(|_| (rnd() % 1000).trailing_zeros() as u8).collect();
    vec![
        ("one", b"x".to_vec()),
        ("two", b"xy".to_vec()),
        ("three", b"xyz".to_vec()),
        ("short", b"abcabcabcabcabcabd".to_vec()),
        ("zeros", vec![0; 200_000]),
        ("text", text),
        ("random", random),
        ("mixed", mixed),
        ("far", far),
        ("skewed", skewed),
    ]
}

#[test]
fn port_equals_7zip_level_5() {
    if Command::new("7z").arg("i").output().is_err() {
        eprintln!("no 7z on PATH; skipping the comparison with 7-Zip");
        return;
    }
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("deflate7z");
    std::fs::create_dir_all(&dir).unwrap();
    let mut failures = Vec::new();
    let mut zipped = 0;
    for (name, data) in inputs() {
        let ours = deflate(&data);
        let mut theirs = vec![("gzip", seven_zip_gzip_stream(&dir, name, &data))];
        if let Some(z) = seven_zip_zip_stream(&dir, name, &data) {
            theirs.push(("zip", z));
            zipped += 1;
        }
        for (kind, t) in theirs {
            if ours != t {
                let at = ours.iter().zip(&t).position(|(a, b)| a != b).unwrap_or(ours.len().min(t.len()));
                failures.push(format!(
                    "{name} ({} bytes, {kind}): ours {} vs 7-Zip {} bytes, first difference at {at}",
                    data.len(),
                    ours.len(),
                    t.len()
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // the zip handler stores what does not shrink (the tiny and the random inputs)
    assert!(zipped >= 5, "7-Zip deflated only {zipped} of the zip entries");
}

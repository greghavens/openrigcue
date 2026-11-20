# Local modifications to libsqlite3-sys 0.30.1

This directory is the crates.io `libsqlite3-sys` 0.30.1 package with its Rust API unchanged. Its
`LICENSE` is retained and also shipped as `LICENSES/libsqlite3-sys-0.30.1.txt`. The upstream package
manifest is kept as `Cargo.toml.orig`.

Changes:

- The bundled `sqlite3.c`, `sqlite3.h` and `sqlite3ext.h` are SQLite 3.50.4 (public domain), from
  `sbooth/CSQLite` tag `3.50.4`, checked against SQLite's official release SHA3-256
  `9145255e83da6529e70121ee4d7a4c88fe83ca4511da0c9ed13d10842df36782`
  (<https://www.sqlite.org/releaselog/3_50_4.html>). The three version/source constants of the
  bundled bindings are updated to match.
- The build does not enable foreign keys by default. It enables FTS3/4/5, math functions and RTree,
  and drops the other optional upstream feature flags.
- The SQLCipher sources and bindings are kept unused. SQLite automatic initialization stays enabled.

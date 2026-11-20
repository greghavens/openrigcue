# Local modifications to lewton 0.10.2

This directory is the crates.io `lewton` 0.10.2 package (crate SHA-256
`777b48df9aaab155475a83a7df3070395ea1ac6902f5cd062b8f2b028075c030`) with changes made for
OpenRigCue. Its MIT/Apache-2.0 `LICENSE` and the source copyright notices are retained; the Xiph BSD
notice for the adapted MDCT stages is in `COPYING-Xiph`. Binary packages also ship
`LICENSES/Lewton-0.10.2.txt` and `LICENSES/Xiph-Vorbis.txt`. The upstream package manifest is kept as
`Cargo.toml.orig`; the local manifest keeps only the synchronous Ogg dependencies OpenRigCue uses.

Changes, all opt-in (default constructors and setup readers keep upstream behavior):

- `IdentHeader::set_window_slope` and `IdentHeader::set_mdct_twiddle_factors` (`src/header.rs`) let
  the caller supply windows and MDCT tables of the correct lengths. The MDCT table override keeps
  Lewton's B/2 and C*2 normalization.
- `IdentHeader::set_native_mdct` selects the transform in `src/native_mdct.rs`, which adapts Xiph's
  published MDCT stages, for either legal block size.
- `read_header_setup_native`, `read_headers_native` and `OggStreamReader::new_native` use a
  double-scale packed-float conversion for codebook minima and deltas, keep that choice across
  chained streams, reject floor type 0, and use the vendored ogg crate's `new_native` packet reader.

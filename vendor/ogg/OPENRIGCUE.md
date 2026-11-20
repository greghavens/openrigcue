# Local modifications to ogg 0.8.0

This directory is the crates.io `ogg` 0.8.0 package (crate archive SHA-256
`6951b4e8bf21c8193da321bcce9c9dd2e13c858fe078bf9054a288b419ae5d6e`) with changes made for
OpenRigCue. Its BSD license and copyright notices remain in `LICENSE` and are also shipped as
`LICENSES/Ogg-BSD.txt`. The upstream package manifest is kept as `Cargo.toml.orig`.

Changes:

- `Packet::discontinuity()` reports a gap, repeated page number or reversed page sequence before a
  returned packet. Continuity is tracked per logical stream, wraps at 32 bits, survives partial pages
  and is reset by seeking. The page parser and the default packet output are unchanged.
- The opt-in constructors `PacketReader::new_native` and `BasePacketReader::new_native` discard
  incomplete packets across missing pages, skip their orphan continuation fragments and skip pages
  that fail their checksum. The existing constructors keep upstream behavior.

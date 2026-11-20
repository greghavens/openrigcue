// The .rsrc section of the DLLs (0x1e0 bytes): the resource tree with its one
// resource, RT_MANIFEST (24) / ID 2 (ISOLATIONAWARE_MANIFEST_RESOURCE_ID) /
// language 0x409, the data entry, and the 0x17d-byte asInvoker manifest
// (res/ets2-telemetry.manifest) at offset 0x60.
// Written out here rather than by windres, which places the data at 0x58;
// with a single .rsrc input GNU ld keeps the section as it is.
    .section .rsrc,"dr"
    .p2align 4
    .long 0, 0                          // root: Characteristics, TimeDateStamp
    .short 0, 0, 0, 1                   // version 0.0, 0 named, 1 ID entry
    .long 24, 0x80000018                // RT_MANIFEST -> subdirectory 0x18
    .long 0, 0
    .short 0, 0, 0, 1
    .long 2, 0x80000030                 // ID 2 -> subdirectory 0x30
    .long 0, 0
    .short 0, 0, 0, 1
    .long 0x409, 0x48                   // language 0x409 -> data entry 0x48
    .rva .Lmanifest                     // data entry: OffsetToData (RVA),
    .long .Lmanifest_end - .Lmanifest   // Size, CodePage, Reserved
    .long 0, 0
    .p2align 4
.Lmanifest:

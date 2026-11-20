/* Section layout of the plugin DLLs: .text, .rdata
   (import address table first, then constants, load-config and debug data,
   unwind data, the import directory with its lookup and name tables, and
   the export directory), .data (initialised data followed by the
   zero-initialised tail), .pdata (x64 only), .rsrc and .reloc. No .eh_frame,
   .CRT, .tls, .bss, .idata or .edata sections: any .ctors/.dtors entries
   (which only the mingw-w64 CRT would run) and the DWARF unwind tables are
   discarded; rustc's rsbegin.o / rsend.o are kept out of the link by
   scripts/pe-link.sh.

   The import tables are src/imports_*.s (.ets2iat, .ets2idata); no import
   library member may be linked, which the ASSERT below checks. GNU ld fills
   the import, IAT and export data directories only from sections named
   .idata/.edata, so scripts/pe-link.sh (run as gcc's -wrapper by build.rs)
   sets them, the load-configuration and the debug directories from the
   symbols here and in the assembly, using the link map.
   Used by build.rs for the release DLL only. */
SECTIONS
{
  . = SIZEOF_HEADERS;
  . = ALIGN(__section_alignment__);
  .text __image_base__ + __section_alignment__ :
  {
    *(.text)
    *(SORT(.text$*))
    *(.text.*)
  }
  .rdata BLOCK(__section_alignment__) :
  {
    KEEP (*(.ets2iat))
    *(.rdata)
    *(SORT(.rdata$*))
    KEEP (*(.xdata*))
    KEEP (*(.ets2idata))
    . = ALIGN(16);
    ets2_export_directory = .;
    KEEP (*(.edata))
    ets2_export_directory_end = .;
    ets2_stray_idata = .;
    *(.idata$*)
    ets2_stray_idata_end = .;
  }
  ASSERT (ets2_stray_idata_end == ets2_stray_idata,
          "an import library member was linked; every import must be defined in src/imports_*.s")
  .data BLOCK(__section_alignment__) :
  {
    *(.data)
    *(SORT(.data$*))
    *(.bss)
    *(COMMON)
  }
  .pdata BLOCK(__section_alignment__) :
  {
    KEEP (*(.pdata*))
  }
  .rsrc BLOCK(__section_alignment__) : SUBALIGN(4)
  {
    KEEP (*(.rsrc))
    KEEP (*(.rsrc$*))
  }
  .reloc BLOCK(__section_alignment__) :
  {
    *(.reloc)
  }
  /DISCARD/ :
  {
    *(.ctors) *(.ctors.*) *(.dtors) *(.dtors.*)
    *(.eh_frame*)
    *(.debug$S) *(.debug$T) *(.debug$F)
    *(.drectve)
    *(.note.GNU-stack)
    *(.gnu.lto_*)
    *(.debug_*)
  }
}

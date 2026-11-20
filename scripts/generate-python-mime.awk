# Generate the pinned default MIME type table as Rust.
# Usage: LC_ALL=C awk -f scripts/generate-python-mime.awk SOURCE > OUTPUT
BEGIN { FS = "'"; in_types = 0; count = 0; found = 0; failed = 0 }
/^    types_map = _types_map_default = [{]/ { in_types = 1; next }
in_types && /^[ ]*[}]/ { found = 1; exit }
in_types && /^[ ]*'/ {
    if (NF < 5 || $2 !~ /^[.]/ || $2 ~ /["\\]/ || $4 !~ /^[[:alnum:]!#$&^_.+-]+\/[[:alnum:]!#$&^_.+-]+$/) {
        print "unsupported MIME table entry: " $0 > "/dev/stderr"
        failed = 1
        exit 1
    }
    if (!($2 in types)) keys[++count] = $2
    types[$2] = $4
}
END {
    if (failed || !found || count < 100) {
        print "incomplete CPython MIME table" > "/dev/stderr"
        exit 1
    }
    for (i = 2; i <= count; i++) {
        key = keys[i]
        j = i - 1
        while (j > 0 && ("@" keys[j]) > ("@" key)) {
            keys[j + 1] = keys[j]
            j--
        }
        keys[j + 1] = key
    }
    print "// Generated from CPython v3.13.13 Lib/mimetypes.py strict defaults."
    print "// Blob: 2af7c4b7b80822238ec3a860b154a7255cedd6d1; PSF license/notice in LICENSES/."
    print "// Regenerate with scripts/generate-python-mime.awk, then rustfmt."
    printf "pub(super) const TYPES: [(&str, &str); %d] = [\n", count
    for (i = 1; i <= count; i++) printf "    (\"%s\", \"%s\"),\n", keys[i], types[keys[i]]
    print "];"
}

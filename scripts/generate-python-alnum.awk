# UnicodeData-15.1.0.txt -> crates/openrigcue-model/src/python_alnum_ranges.rs.
# Run with awk -f scripts/generate-python-alnum.awk INPUT, then cargo fmt --all.
BEGIN {
    FS = ";"
    print "// Generated from Unicode 15.1.0; see scripts/generate-python-alnum.awk."
    print "// Unicode data license: ../../../LICENSES/Unicode.txt."
    print "const ALNUM_RANGES: &[(u32, u32)] = &["
}
function append_range(start, finish) {
    if (have_range && start == range_end + 1) {
        range_end = finish
    } else {
        if (have_range) printf "    (0x%X, 0x%X),\n", range_start, range_end
        range_start = start
        range_end = finish
        have_range = 1
    }
}
NF {
    point = strtonum("0x" $1)
    wanted = $3 ~ /^L/ || $7 != "" || $8 != "" || $9 != ""
    if ($2 ~ /, First>$/) {
        first = point
        first_wanted = wanted
        in_range = 1
    } else if ($2 ~ /, Last>$/) {
        if (!in_range || first_wanted != wanted) exit 1
        if (wanted) append_range(first, point)
        in_range = 0
    } else {
        if (in_range) exit 1
        if (wanted) append_range(point, point)
    }
}
END {
    if (in_range) exit 1
    if (have_range) printf "    (0x%X, 0x%X),\n", range_start, range_end
    print "];"
}

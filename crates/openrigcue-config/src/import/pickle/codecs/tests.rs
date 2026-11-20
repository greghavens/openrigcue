//! Results and errors of the pickle `str(...)` constructor.

use super::super::{Dict, LegacyValue, PythonString};
use super::{construct_str, str_new, NativeException, UnicodeErrorObject};
use openrigcue_model::native_value::NativeObject;

fn bytes(data: &[u8]) -> LegacyValue {
    LegacyValue::Bytes(data.to_vec())
}

fn text(value: &str) -> LegacyValue {
    LegacyValue::Str(value.into())
}

fn points(values: &[u32]) -> LegacyValue {
    LegacyValue::from_python_string(PythonString::from_codepoints(values.to_vec()).unwrap())
}

fn summary(exception: &NativeException) -> (&'static str, String) {
    (exception.class(), exception.message().scalar_string().expect("scalar message"))
}

fn call(args: &[LegacyValue]) -> Result<LegacyValue, (&'static str, String)> {
    str_new(args, None).map_err(|error| summary(&error))
}

fn decoded(data: &[u8], encoding: &str) -> LegacyValue {
    call(&[bytes(data), text(encoding)]).unwrap_or_else(|error| panic!("{data:?} {encoding}: {error:?}"))
}

fn handled(data: &[u8], encoding: &str, errors: &str) -> LegacyValue {
    call(&[bytes(data), text(encoding), text(errors)]).unwrap_or_else(|error| panic!("{data:?} {encoding}: {error:?}"))
}

fn fails(args: &[LegacyValue]) -> (&'static str, String) {
    call(args).expect_err("raises")
}

fn decode_fails(data: &[u8], encoding: &str) -> String {
    let (class, message) = fails(&[bytes(data), text(encoding)]);
    assert_eq!(class, "UnicodeDecodeError", "{message}");
    message
}

fn keywords(pairs: &[(&str, LegacyValue)]) -> Dict {
    let mut dict = Dict::new();
    for (key, value) in pairs {
        dict.insert(*key, value.clone());
    }
    dict
}

#[test]
fn unary_forms_convert_and_keep_exact_strings() {
    assert_eq!(call(&[]), Ok(text("")));
    assert_eq!(call(&[bytes(b"ab")]), Ok(text("b'ab'")));
    assert_eq!(call(&[LegacyValue::Int(5)]), Ok(text("5")));
    let shared = LegacyValue::Object(NativeObject::new(text("same")));
    assert!(call(std::slice::from_ref(&shared)).unwrap().same_object(&shared));
    let lone = points(&[0xD800]);
    assert_eq!(call(std::slice::from_ref(&lone)), Ok(lone.clone()));
}

#[test]
fn argument_count_duplicate_and_keyword_errors() {
    let four = [bytes(b""), text("utf-8"), text("strict"), text("x")];
    assert_eq!(fails(&four), ("TypeError", "str() takes at most 3 arguments (4 given)".into()));
    let many = keywords(&[
        ("a", LegacyValue::None),
        ("b", LegacyValue::None),
        ("c", LegacyValue::None),
        ("d", LegacyValue::None),
    ]);
    assert_eq!(
        summary(&str_new(&[], Some(&many)).unwrap_err()),
        ("TypeError", "str() takes at most 3 keyword arguments (4 given)".into())
    );
    let duplicate = keywords(&[("object", bytes(b"x"))]);
    assert_eq!(
        summary(&str_new(&[bytes(b"x")], Some(&duplicate)).unwrap_err()),
        ("TypeError", "argument for str() given by name ('object') and position (1)".into())
    );
    let typo = keywords(&[("encodin", text("utf-8"))]);
    assert_eq!(
        summary(&str_new(&[bytes(b"x")], Some(&typo)).unwrap_err()),
        ("TypeError", "str() got an unexpected keyword argument 'encodin'. Did you mean 'encoding'?".into())
    );
    let case = keywords(&[("Errors", text("strict"))]);
    assert_eq!(
        summary(&str_new(&[], Some(&case)).unwrap_err()),
        ("TypeError", "str() got an unexpected keyword argument 'Errors'. Did you mean 'errors'?".into())
    );
    let unknown = keywords(&[("foo", LegacyValue::None)]);
    assert_eq!(
        summary(&str_new(&[], Some(&unknown)).unwrap_err()),
        ("TypeError", "str() got an unexpected keyword argument 'foo'".into())
    );
    let mut numeric = Dict::new();
    numeric.insert(super::Key::Int(1), LegacyValue::None);
    assert_eq!(summary(&str_new(&[], Some(&numeric)).unwrap_err()), ("TypeError", "keywords must be strings".into()));
    let all = keywords(&[("errors", text("strict")), ("object", bytes(b"\xc3\xa9")), ("encoding", text("utf-8"))]);
    assert_eq!(str_new(&[], Some(&all)), Ok(text("é")));
    // An encoding without an object still gives the empty string; an empty keyword dict is no keywords.
    assert_eq!(str_new(&[], Some(&keywords(&[("encoding", text("nope"))]))), Ok(text("")));
    assert_eq!(str_new(&[bytes(b"x")], Some(&Dict::new())), Ok(text("b'x'")));
}

#[test]
fn encoding_and_errors_arguments_are_checked_first() {
    assert_eq!(
        fails(&[bytes(b"a"), LegacyValue::Int(1)]),
        ("TypeError", "str() argument 'encoding' must be str, not int".into())
    );
    assert_eq!(
        fails(&[bytes(b"a"), text("utf-8"), LegacyValue::None]),
        ("TypeError", "str() argument 'errors' must be str, not None".into())
    );
    assert_eq!(fails(&[bytes(b"a"), text("utf\u{0}8")]), ("ValueError", "embedded null character".into()));
    assert_eq!(
        fails(&[bytes(b"a"), points(&[0x61, 0xDC80, 0xDC81])]),
        ("UnicodeEncodeError", "'utf-8' codec can't encode characters in position 1-2: surrogates not allowed".into())
    );
    assert_eq!(
        fails(&[bytes(b"a"), points(&[0xDC80])]),
        (
            "UnicodeEncodeError",
            "'utf-8' codec can't encode character '\\udc80' in position 0: surrogates not allowed".into()
        )
    );
    assert_eq!(fails(&[text("a"), text("utf-8")]), ("TypeError", "decoding str is not supported".into()));
    assert_eq!(
        fails(&[LegacyValue::Int(1), text("utf-8")]),
        ("TypeError", "decoding to str: need a bytes-like object, int found".into())
    );
    assert_eq!(call(&[LegacyValue::ByteArray(b"ok".to_vec()), text("ascii")]), Ok(text("ok")));
}

#[test]
fn lookup_and_empty_input() {
    assert_eq!(fails(&[bytes(b"x"), text("nope")]), ("LookupError", "unknown encoding: nope".into()));
    assert_eq!(
        fails(&[bytes(b"x"), text("base64")]),
        ("LookupError", "'base64' is not a text encoding; use codecs.decode() to handle arbitrary codecs".into())
    );
    assert_eq!(call(&[bytes(b""), text("nope"), text("bogus")]), Ok(text("")));
    assert_eq!(fails(&[bytes(b"x"), text("undefined")]), ("UnicodeError", "undefined encoding".into()));
    assert_eq!(decoded(b"\xe9", "Latin-1"), text("é"));
    assert_eq!(decoded(b"\xe9", "l1"), text("é"));
}

#[test]
fn utf8_errors_and_handlers() {
    assert_eq!(decoded(b"caf\xc3\xa9 \xf0\x9f\x98\x80", "UTF-8"), text("café 😀"));
    assert_eq!(
        decode_fails(b"\xff", "utf-8"),
        "'utf-8' codec can't decode byte 0xff in position 0: invalid start byte"
    );
    assert_eq!(
        decode_fails(b"a\xe2\x82", "utf8"),
        "'utf-8' codec can't decode bytes in position 1-2: unexpected end of data"
    );
    assert_eq!(
        decode_fails(b"\xed\xa0\x80", "utf-8"),
        "'utf-8' codec can't decode byte 0xed in position 0: invalid continuation byte"
    );
    assert_eq!(
        decode_fails(b"\xf0\x9f\x98x", "utf-8"),
        "'utf-8' codec can't decode bytes in position 0-2: invalid continuation byte"
    );
    assert_eq!(handled(b"\xed\xa0\x80z", "utf-8", "surrogatepass"), points(&[0xD800, 0x7A]));
    assert_eq!(handled(b"a\xff\xfe", "utf-8", "surrogateescape"), points(&[0x61, 0xDCFF, 0xDCFE]));
    assert_eq!(handled(b"a\xffb", "utf-8", "replace"), text("a\u{fffd}b"));
    assert_eq!(handled(b"a\xffb", "utf-8", "ignore"), text("ab"));
    assert_eq!(handled(b"a\xffb", "utf-8", "backslashreplace"), text("a\\xffb"));
    assert_eq!(handled(b"ok", "utf-8", "bogus"), text("ok"));
    assert_eq!(
        fails(&[bytes(b"\xff"), text("utf-8"), text("bogus")]),
        ("LookupError", "unknown error handler name 'bogus'".into())
    );
    assert_eq!(
        fails(&[bytes(b"\xff"), text("utf-8"), text("xmlcharrefreplace")]),
        ("TypeError", "don't know how to handle UnicodeDecodeError in error callback".into())
    );
    assert_eq!(
        fails(&[bytes(b"\xff"), text("ascii"), text("surrogatepass")]),
        ("UnicodeDecodeError", "'ascii' codec can't decode byte 0xff in position 0: ordinal not in range(128)".into())
    );
    assert_eq!(decoded(b"\xef\xbb\xbfa", "utf-8-sig"), text("a"));
    assert_eq!(decoded(b"\xef\xbb\xbfa", "utf-8"), text("\u{feff}a"));
    assert_eq!(
        decode_fails(b"\xef\xbb\xbf\xff", "utf_8_sig"),
        "'utf-8' codec can't decode byte 0xff in position 0: invalid start byte"
    );
    // Errors with no encoding decode UTF-8.
    assert_eq!(call(&[bytes(b"\xffa"), LegacyValue::None]).unwrap_err().0, "TypeError");
    let only_errors = keywords(&[("errors", text("replace"))]);
    assert_eq!(str_new(&[bytes(b"\xffa")], Some(&only_errors)), Ok(text("\u{fffd}a")));
}

#[test]
fn utf16_utf32_and_utf7() {
    assert_eq!(decoded(b"\xff\xfea\x00", "utf-16"), text("a"));
    assert_eq!(decoded(b"\xfe\xff\x00a", "utf-16"), text("a"));
    assert_eq!(decoded(b"a\x00", "utf-16"), text("a"));
    assert_eq!(decoded(b"\x3d\xd8\x00\xde", "utf-16-le"), text("😀"));
    assert_eq!(decode_fails(b"a", "utf-16"), "'utf-16-le' codec can't decode byte 0x61 in position 0: truncated data");
    assert_eq!(
        decode_fails(b"\x00\xdc", "utf-16-le"),
        "'utf-16-le' codec can't decode bytes in position 0-1: illegal encoding"
    );
    assert_eq!(
        decode_fails(b"\xd8\x00\x00a", "utf-16-be"),
        "'utf-16-be' codec can't decode bytes in position 0-1: illegal UTF-16 surrogate"
    );
    assert_eq!(
        decode_fails(b"\x00\xd8a", "utf-16-le"),
        "'utf-16-le' codec can't decode bytes in position 0-2: unexpected end of data"
    );
    assert_eq!(handled(b"\x00\xd8a\x00", "utf-16-le", "surrogatepass"), points(&[0xD800, 0x61]));
    assert_eq!(decoded(b"\xff\xfe\x00\x00a\x00\x00\x00", "utf-32"), text("a"));
    assert_eq!(
        decode_fails(b"\x00\x00\x11\x00", "utf-32-le"),
        "'utf-32-le' codec can't decode bytes in position 0-3: code point not in range(0x110000)"
    );
    assert_eq!(
        decode_fails(b"\x00\x00\xd8\x00", "utf-32-be"),
        "'utf-32-be' codec can't decode bytes in position 0-3: code point in surrogate code point range(0xd800, 0xe000)"
    );
    assert_eq!(decoded(b"+AOk-+-a", "utf-7"), text("é+a"));
    assert_eq!(decoded(b"+2D3eAA-", "utf7"), text("😀"));
    assert_eq!(
        decode_fails(b"+AO", "utf-7"),
        "'utf7' codec can't decode bytes in position 0-2: unterminated shift sequence"
    );
    assert_eq!(
        decode_fails(b"a\x80", "utf-7"),
        "'utf7' codec can't decode byte 0x80 in position 1: unexpected special character"
    );
    assert_eq!(decode_fails(b"+\x00", "utf-7"), "'utf7' codec can't decode bytes in position 0-1: ill-formed sequence");
}

#[test]
fn single_byte_tables_and_code_pages() {
    assert_eq!(decoded(b"\x80a", "windows-1252"), text("€a"));
    assert_eq!(
        decode_fails(b"a\x81", "cp1252"),
        "'charmap' codec can't decode byte 0x81 in position 1: character maps to <undefined>"
    );
    assert_eq!(handled(b"\x81", "cp1252", "replace"), text("\u{fffd}"));
    assert_eq!(decoded(b"\x81\x80", "mbcs"), text("\u{81}€"));
    assert_eq!(decoded(b"\x81", "oem"), text("ü"));
    assert_eq!(decoded(b"\xc1", "cp037"), text("A"));
    assert_eq!(decoded(b"\xe9", "koi8-r"), text("И"));
    assert_eq!(decoded(b"\xa0", "mac-arabic"), text(" "));
    assert_eq!(
        decode_fails(b"\x80", "ascii"),
        "'ascii' codec can't decode byte 0x80 in position 0: ordinal not in range(128)"
    );
    assert_eq!(handled(b"\x80", "us-ascii", "surrogateescape"), points(&[0xDC80]));
}

#[test]
fn escape_codecs_and_character_names() {
    assert_eq!(
        decoded(b"\\N{LATIN SMALL LETTER E WITH ACUTE}\\u263a\\x41\\101\\t\\q", "unicode_escape"),
        text("é☺AA\t\\q")
    );
    assert_eq!(decoded(b"\\N{latin small letter a}\\N{BOM}", "unicode-escape"), text("a\u{feff}"));
    assert_eq!(decoded(b"\\N{HANGUL SYLLABLE GAG}\\N{CJK UNIFIED IDEOGRAPH-4E00}", "unicode_escape"), text("각一"));
    assert_eq!(decoded(b"\\N{TANGUT IDEOGRAPH-17000}\\777", "unicode_escape"), text("\u{17000}\u{1ff}"));
    assert_eq!(
        decode_fails(b"\\N{NOPE}", "unicode_escape"),
        "'unicodeescape' codec can't decode bytes in position 0-7: unknown Unicode character name"
    );
    assert_eq!(
        decode_fails(b"\\N{CJK UNIFIED IDEOGRAPH-0E00}", "unicode_escape"),
        "'unicodeescape' codec can't decode bytes in position 0-29: unknown Unicode character name"
    );
    assert_eq!(
        decode_fails(b"\\N{}", "unicode_escape"),
        "'unicodeescape' codec can't decode bytes in position 0-2: malformed \\N character escape"
    );
    assert_eq!(
        decode_fails(b"\\x4", "unicode_escape"),
        "'unicodeescape' codec can't decode bytes in position 0-2: truncated \\xXX escape"
    );
    assert_eq!(
        decode_fails(b"ab\\", "unicode_escape"),
        "'unicodeescape' codec can't decode byte 0x5c in position 2: \\ at end of string"
    );
    assert_eq!(
        decode_fails(b"\\U00110000", "unicode_escape"),
        "'unicodeescape' codec can't decode bytes in position 0-9: illegal Unicode character"
    );
    assert_eq!(handled(b"\\x4z", "unicode_escape", "replace"), text("\u{fffd}z"));
    assert_eq!(decoded(b"\\u00e9\\q\\", "raw_unicode_escape"), text("é\\q\\"));
    assert_eq!(
        decode_fails(b"\\u12", "raw-unicode-escape"),
        "'rawunicodeescape' codec can't decode bytes in position 0-3: truncated \\uXXXX escape"
    );
}

#[test]
fn idna_and_punycode() {
    assert_eq!(decoded(b"xn--caf-dma.Com.", "idna"), text("café.Com."));
    assert_eq!(decoded(b"plain.example", "idna"), text("plain.example"));
    assert_eq!(decoded(b"XN--CAF-DMA", "idna"), text("CAFé"));
    assert_eq!(decoded(b"caf-dma", "punycode"), text("café"));
    assert_eq!(
        fails(&[bytes(b"a"), text("idna"), text("ignore")]),
        ("UnicodeError", "Unsupported error handling: ignore".into())
    );
    assert_eq!(
        decode_fails(b"a.xn--caf-dm!", "idna"),
        "'idna' codec can't decode byte 0x21 in position 12: Invalid extended code point '33'"
    );
    assert_eq!(
        decode_fails(b"abc-d", "punycode"),
        "'punycode' codec can't decode bytes in position 5-5: incomplete punycode string"
    );
    assert_eq!(
        decode_fails(b"\xff-a", "punycode"),
        "'ascii' codec can't decode byte 0xff in position 0: ordinal not in range(128)"
    );
    assert_eq!(handled(b"\xff-a", "punycode", "replace"), text("\u{80}\u{fffd}"));
    assert_eq!(
        decode_fails(b"xn--abc-", "idna"),
        "'idna' codec can't decode bytes in position 0-7: IDNA does not round-trip, 'b'xn--abc-'' != 'b'abc''"
    );
}

#[test]
fn multibyte_codecs() {
    assert_eq!(decoded(b"\x82\xa0a", "shift_jis"), text("あa"));
    assert_eq!(decoded(b"\x87\x40", "cp932"), text("①"));
    assert_eq!(decoded(b"\xa4\xa2", "euc-jp"), text("あ"));
    assert_eq!(decoded(b"\xa4\xa2", "euc_jis_2004"), text("あ"));
    assert_eq!(decoded(b"\x82\xa0", "shift_jis_2004"), text("あ"));
    assert_eq!(
        decode_fails(b"\xae\xa1", "euc_jisx0213"),
        "'euc_jisx0213' codec can't decode bytes in position 0-1: illegal multibyte sequence"
    );
    assert!(call(&[bytes(b"\xae\xa1"), text("euc_jis_2004")]).is_ok());
    assert_eq!(decoded(b"\xc4\xe3", "gbk"), text("你"));
    assert_eq!(decoded(b"\xc4\xe3", "gb2312"), text("你"));
    assert_eq!(decoded(b"\x81\x30\x81\x30\x90\x30\x81\x30", "gb18030"), text("\u{80}\u{10000}"));
    assert_eq!(decoded(b"~{Dc~}~~", "hz"), text("你~"));
    assert_eq!(decoded(b"\xa7\x41", "big5"), text("你"));
    assert_eq!(decoded(b"\xa7\x41", "cp950"), text("你"));
    assert_eq!(decoded(b"\x88\x62", "big5hkscs"), text("\u{ca}\u{304}"));
    assert_eq!(decoded(b"\xc7\xd1", "euc_kr"), text("한"));
    assert_eq!(decoded(b"\xc7\xd1", "cp949"), text("한"));
    assert_eq!(decoded(b"\x1b$B$\"\x1b(B", "iso2022_jp"), text("あ"));
    assert_eq!(decoded(b"\x1b$)C\x0eGQ\x0f", "iso2022_kr"), text("한"));
    assert_eq!(
        decode_fails(b"\x82", "shift_jis"),
        "'shift_jis' codec can't decode byte 0x82 in position 0: incomplete multibyte sequence"
    );
    assert_eq!(
        decode_fails(b"a\x82 ", "sjis"),
        "'shift_jis' codec can't decode byte 0x82 in position 1: illegal multibyte sequence"
    );
    assert_eq!(handled(b"\x82 a", "shift_jis", "replace"), text("\u{fffd} a"));
    assert_eq!(handled(b"\x82 a", "shift_jis", "backslashreplace"), text("\\x82 a"));
    assert_eq!(
        decode_fails(b"\x1b$Bx", "iso2022_jp"),
        "'iso2022_jp' codec can't decode byte 0x78 in position 3: incomplete multibyte sequence"
    );
}

fn native_fails(args: &[LegacyValue]) -> NativeException {
    str_new(args, None).expect_err("raises")
}

fn python(text: &str) -> PythonString {
    PythonString::from(text)
}

fn attributes(exception: &NativeException) -> (String, Vec<u8>, i64, i64, String) {
    let attributes = exception.unicode_error().expect("UnicodeError attributes");
    let UnicodeErrorObject::Bytes(object) = &attributes.object else { panic!("decode object") };
    (
        attributes.encoding.scalar_string().unwrap(),
        object.clone(),
        attributes.start,
        attributes.end,
        attributes.reason.scalar_string().unwrap(),
    )
}

#[test]
fn registry_codecs_add_the_decoding_note_and_shortcuts_do_not() {
    let failure = native_fails(&[bytes(b"\x81"), text("cp1252")]);
    assert_eq!(failure.notes(), &[python("decoding with 'cp1252' codec failed")]);
    assert_eq!(
        failure.message(),
        &python("'charmap' codec can't decode byte 0x81 in position 0: character maps to <undefined>")
    );
    assert_eq!(
        attributes(&failure),
        ("charmap".into(), b"\x81".to_vec(), 0, 1, "character maps to <undefined>".into())
    );
    assert_eq!(
        failure.format_exception_only(),
        python(
            "UnicodeDecodeError: 'charmap' codec can't decode byte 0x81 in position 0: character maps to <undefined>\n\
             decoding with 'cp1252' codec failed\n"
        )
    );
    assert!(failure.context().is_none());
    // The codecs handled before the registry fail without a note.
    assert!(native_fails(&[bytes(b"\xff"), text("UTF-8")]).notes().is_empty());
    assert!(native_fails(&[bytes(b"\xff"), text("latin-1-x")]).notes().is_empty(), "unknown encoding: no codec ran");
    // utf_8_sig decodes the data after the BOM: the attributes hold that slice; the note names the argument.
    let sig = native_fails(&[bytes(b"\xef\xbb\xbfa\xff"), text("UTF-8-SIG")]);
    assert_eq!(sig.notes(), &[python("decoding with 'UTF-8-SIG' codec failed")]);
    assert_eq!(attributes(&sig), ("utf-8".into(), b"a\xff".to_vec(), 1, 2, "invalid start byte".into()));
    // Handler lookups and other errors raised inside a registry codec carry the note too.
    let handler = native_fails(&[bytes(b"\x81"), text("cp1252"), text("bogus")]);
    assert_eq!(
        (handler.class(), handler.notes()),
        ("LookupError", &[python("decoding with 'cp1252' codec failed")][..])
    );
    assert!(native_fails(&[bytes(b"\xff"), text("utf-8"), text("bogus")]).notes().is_empty());
    let undefined = native_fails(&[bytes(b"x"), text("undefined")]);
    assert_eq!(undefined.notes(), &[python("decoding with 'undefined' codec failed")]);
    assert_eq!(undefined.to_string(), "UnicodeError: undefined encoding");
}

#[test]
fn idna_and_punycode_reraises_keep_their_context_chain() {
    let outer = native_fails(&[bytes(b"a.xn--caf-dm!"), text("idna")]);
    let reason = "Invalid extended code point '33'";
    assert_eq!(attributes(&outer), ("idna".into(), b"a.xn--caf-dm!".to_vec(), 12, 13, reason.into()));
    assert_eq!(outer.notes(), &[python("decoding with 'idna' codec failed")]);
    assert!(!outer.suppress_context());
    let label = outer.context().expect("ToUnicode's re-raise");
    assert_eq!(attributes(label), ("idna".into(), b"xn--caf-dm!".to_vec(), 10, 11, reason.into()));
    assert!(label.notes().is_empty() && !label.suppress_context());
    let punycode = label.context().expect("label1.decode('punycode')");
    assert_eq!(attributes(punycode), ("punycode".into(), b"caf-dm!".to_vec(), 6, 7, reason.into()));
    assert_eq!(punycode.notes(), &[python("decoding with 'punycode' codec failed")]);
    assert!(punycode.suppress_context(), "raise ... from None");
    let inner = punycode.context().expect("insertion_sort's exception");
    assert_eq!(attributes(inner), ("punycode".into(), b"DM!".to_vec(), 2, 3, reason.into()));
    assert!(inner.context().is_none() && inner.notes().is_empty());

    let ascii = native_fails(&[bytes(b"\xff-a"), text("punycode")]);
    assert_eq!(attributes(&ascii), ("ascii".into(), b"\xff-a".to_vec(), 0, 1, "ordinal not in range(128)".into()));
    assert!(ascii.suppress_context());
    assert_eq!(attributes(ascii.context().unwrap()).1, b"\xff".to_vec());

    // U+E000 (Punycode "0y0c") is prohibited: the ToASCII encode error becomes the context.
    let prohibited = native_fails(&[bytes(b"xn--0y0c"), text("idna")]);
    assert_eq!(
        prohibited.message(),
        &python("'idna' codec can't decode byte 0x78 in position 0: Invalid character '\\ue000'")
    );
    let encode = prohibited.context().expect("ToASCII");
    assert_eq!(encode.class(), "UnicodeEncodeError");
    let attributes = encode.unicode_error().unwrap();
    assert_eq!(attributes.object, UnicodeErrorObject::Text(PythonString::from_codepoints(vec![0xE000]).unwrap()));
    assert_eq!((attributes.start, attributes.end), (0, 1));
    assert_eq!(
        encode.message(),
        &python("'idna' codec can't encode character '\\ue000' in position 0: Invalid character '\\ue000'")
    );
}

#[test]
fn surrogate_keywords_keep_their_code_points_until_an_output_boundary() {
    let mut kwargs = Dict::new();
    let keyword = PythonString::from_codepoints(vec![0x61, 0xDC80]).unwrap();
    kwargs.insert(super::Key::Extra(openrigcue_model::native_value::NativeKey::Unicode(keyword)), LegacyValue::None);
    let failure = str_new(&[], Some(&kwargs)).unwrap_err();
    let mut expected: Vec<u32> = "str() got an unexpected keyword argument 'a".chars().map(u32::from).collect();
    expected.extend([0xDC80, u32::from('\'')]);
    assert_eq!((failure.class(), failure.message()), ("TypeError", &PythonString::from_codepoints(expected).unwrap()));
    assert_eq!(failure.to_string(), "TypeError: str() got an unexpected keyword argument 'a\\udc80'");
    let strict = super::utf8_strict(failure.message()).unwrap_err();
    assert_eq!(
        summary(&strict),
        (
            "UnicodeEncodeError",
            "'utf-8' codec can't encode character '\\udc80' in position 43: surrogates not allowed".into()
        )
    );
}

#[test]
fn the_parent_error_carries_the_native_payload() {
    match construct_str(&[bytes(b"\x81"), text("cp1252")], None) {
        Err(super::PickleError::Native(exception)) => {
            assert_eq!(exception.notes(), &[python("decoding with 'cp1252' codec failed")]);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn reduce_reaches_the_constructor_with_its_native_exception() {
    // protocol 3: GLOBAL builtins.str, SHORT_BINBYTES, SHORT_BINUNICODE, TUPLE2, REDUCE, STOP
    let decoded = super::super::decode(b"\x80\x03cbuiltins\nstr\nC\x02\xc3\xa9X\x05\x00\x00\x00utf-8\x86R.");
    assert_eq!(decoded, Ok(text("é")));
    match super::super::decode(b"\x80\x03cbuiltins\nstr\nC\x01\x81X\x06\x00\x00\x00cp1252\x86R.") {
        Err(error) => {
            let native = error.native().expect("native exception");
            assert_eq!(native.class(), "UnicodeDecodeError");
            assert_eq!(native.notes(), &[python("decoding with 'cp1252' codec failed")]);
        }
        other => panic!("{other:?}"),
    }
}

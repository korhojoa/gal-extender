// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 korhojoa

//! Bounded sensor field readers. Lookup selects the first matching tag and scans for
//! val=. A value can come from a later element. a nonnumeric enum can become zero. Some
//! sensor mappings remain unverified. Input stops at the first NUL. Tests constrain
//! these parser rules.

/// The bytes of `s` up to its first NUL (a C string's view).
#[must_use]
pub fn cstr(s: &[u8]) -> &[u8] {
    match s.iter().position(|&b| b == 0) {
        Some(n) => &s[..n],
        None => s,
    }
}

/// `strstr`: the offset of the first `needle` in `hay`. An empty needle
/// matches at 0.
#[must_use]
pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Concatenate parts into a NUL-terminated buffer. Limit output to capacity minus one
/// bytes.
fn needle(parts: &[&[u8]], cap: usize) -> Vec<u8> {
    let mut v: Vec<u8> = parts.concat();
    v.truncate(cap - 1);
    v
}

/// `xml_attr(xml, attr, out, outlen)`: the value of the first ` attr="`
/// anywhere in `xml` (unbounded), cut at `outlen - 1` bytes. None when the
/// needle or its closing quote is missing.
#[must_use]
pub fn attr(xml: &[u8], name: &[u8], outlen: usize) -> Option<Vec<u8>> {
    let xml = cstr(xml);
    let n = needle(&[b" ", name, b"=\""], 80);
    let p = find(xml, &n)? + n.len();
    let len = xml[p..].iter().position(|&b| b == b'"')?;
    let len = len.min(outlen - 1);
    Some(xml[p..p + len].to_vec())
}

/// `xml_find_elem(xml, tag)`: the offset of `<tag `, else of `<tag>`, else
/// of `<tag/>` (each searched over the whole text, in that order).
#[must_use]
pub fn find_elem(xml: &[u8], tag: &[u8]) -> Option<usize> {
    let xml = cstr(xml);
    for tail in [&b" "[..], b">", b"/>"] {
        let n = needle(&[b"<", tag, tail], 64);
        if let Some(p) = find(xml, &n) {
            return Some(p);
        }
    }
    None
}

/// `state="nodata"` or `state="error"` on the element at `elem` (unbounded
/// search, `sbuf[32]`).
fn bad_state(elem: &[u8]) -> bool {
    matches!(
        attr(elem, b"state", 32).as_deref(),
        Some(b"nodata" | b"error")
    )
}

/// What `xml_scalar` gives: the number (`atof`) and the flag (`val` is
/// "true" or "1").
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scalar {
    pub num: f64,
    pub flag: bool,
}

/// `xml_scalar(xml, tag, &val, &bool)`: the first element of `tag` with a
/// `val` (`vbuf[64]`) and no nodata/error state.
#[must_use]
pub fn scalar(xml: &[u8], tag: &[u8]) -> Option<Scalar> {
    let xml = cstr(xml);
    let p = find_elem(xml, tag)?;
    let v = attr(&xml[p..], b"val", 64)?;
    if bad_state(&xml[p..]) {
        return None;
    }
    Some(Scalar {
        num: atof(&v),
        flag: v == b"true" || v == b"1",
    })
}

/// The offsets of the elements that carry `name="fname"`, each backed up to
/// its `<` (or to the start of the text), in text order.
fn named(xml: &[u8], fname: &[u8]) -> Vec<usize> {
    let n = needle(&[b"name=\"", fname, b"\""], 80);
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = find(&xml[from..], &n) {
        let at = from + i;
        let mut elem = at;
        while elem > 0 && xml[elem] != b'<' {
            elem -= 1;
        }
        out.push(elem);
        from = at + n.len();
    }
    out
}

/// `xml_obj_field(xml, fname, &val)`: the `atof` of the first element named
/// `fname` that has a `val` (unbounded) and no nodata/error state.
#[must_use]
pub fn obj_field(xml: &[u8], fname: &[u8]) -> Option<f64> {
    let xml = cstr(xml);
    for elem in named(xml, fname) {
        if let Some(v) = attr(&xml[elem..], b"val", 64) {
            if !bad_state(&xml[elem..]) {
                return Some(atof(&v));
            }
        }
    }
    None
}

/// `xml_obj_field_str(xml, fname, out, outlen)`: the `val` text, as
/// [`obj_field`] picks the element.
#[must_use]
pub fn obj_field_str(xml: &[u8], fname: &[u8], outlen: usize) -> Option<Vec<u8>> {
    let xml = cstr(xml);
    for elem in named(xml, fname) {
        if let Some(v) = attr(&xml[elem..], b"val", outlen) {
            if !bad_state(&xml[elem..]) {
                return Some(v);
            }
        }
    }
    None
}

/// C `atof` (`strtod` without the end pointer): leading white space, then
/// the longest decimal prefix (`[+-]digits[.digits][e[+-]digits]`, at least
/// one digit), or `inf`/`infinity`/`nan`. 0.0 when there is none. Hex
/// floats are read as their leading `0` (EXLAP sends none).
#[must_use]
pub fn atof(s: &[u8]) -> f64 {
    let s = cstr(s);
    let mut i = 0;
    while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
        i += 1;
    }
    let start = i;
    let neg = match s.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let rest = &s[i..];
    let lower: Vec<u8> = rest.iter().take(8).map(u8::to_ascii_lowercase).collect();
    if lower.starts_with(b"inf") {
        return if neg {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    if lower.starts_with(b"nan") {
        return f64::NAN;
    }
    let mut j = i;
    let mut digits = 0;
    while j < s.len() && s[j].is_ascii_digit() {
        j += 1;
        digits += 1;
    }
    if j < s.len() && s[j] == b'.' {
        j += 1;
        while j < s.len() && s[j].is_ascii_digit() {
            j += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return 0.0;
    }
    if j < s.len() && (s[j] == b'e' || s[j] == b'E') {
        let mut k = j + 1;
        if k < s.len() && (s[k] == b'+' || s[k] == b'-') {
            k += 1;
        }
        let e0 = k;
        while k < s.len() && s[k].is_ascii_digit() {
            k += 1;
        }
        if k > e0 {
            j = k;
        }
    }
    core::str::from_utf8(&s[start..j])
        .ok()
        .and_then(|t| t.parse::<f64>().ok())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atof_is_c_atof() {
        assert!((atof(b".58") - 0.58).abs() < 1e-12);
        assert!((atof(b"-.375") + 0.375).abs() < 1e-12);
        assert!((atof(b"9.89E+7") - 9.89e7).abs() < 1.0);
        assert!((atof(b"  15xyz") - 15.0).abs() < 1e-12);
        assert!((atof(b"1e") - 1.0).abs() < 1e-12);
        assert_eq!(atof(b"left").to_bits(), 0.0_f64.to_bits());
        assert_eq!(atof(b"true").to_bits(), 0.0_f64.to_bits());
        assert_eq!(atof(b"").to_bits(), 0.0_f64.to_bits());
        assert_eq!(atof(b"-").to_bits(), 0.0_f64.to_bits());
        assert!(atof(b"INF").is_infinite());
    }

    #[test]
    fn missing_value_uses_later_element_value() {
        // xml_attr is an unbounded strstr: the first element has no val, so
        // the value of the NEXT element is read, and its state too.
        let x = b"<Dat url=\"u\"><Abs name=\"a\"/><Abs name=\"b\" val=\"7\"/></Dat>";
        assert_eq!(obj_field(x, b"a"), Some(7.0));
        let y =
            b"<Dat url=\"u\"><Abs name=\"a\" state=\"error\"/><Abs name=\"b\" val=\"7\"/></Dat>";
        assert_eq!(obj_field(y, b"a"), None);
        assert_eq!(obj_field(y, b"b"), Some(7.0));
    }

    #[test]
    fn find_elem_prefers_the_space_form() {
        let x = b"<Abs>1</Abs><Abs val=\"2\"/>";
        assert_eq!(find_elem(x, b"Abs"), Some(12));
        assert_eq!(scalar(x, b"Abs").map(|s| s.num), Some(2.0));
    }

    #[test]
    fn attr_cuts_like_the_c_buffers() {
        let x = b"<Dat url=\"abcdefghij\">";
        assert_eq!(attr(x, b"url", 5).as_deref(), Some(&b"abcd"[..]));
        assert_eq!(attr(b"<a b=\"x\0\"/>", b"b", 64), None);
    }
}

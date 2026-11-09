//! Frozen MIME decisions from the deployed Python 3.13 image.
//! Runtime interpretation follows POSIX mimetypes.guess_type, without Python.
use serde::Deserialize;
use std::{collections::BTreeMap, sync::OnceLock};
#[derive(Deserialize)]
struct Maps {
    types_map: BTreeMap<String, String>,
    suffix_map: BTreeMap<String, String>,
    encodings_map: BTreeMap<String, String>,
}
fn maps() -> &'static Maps {
    static MAPS: OnceLock<Maps> = OnceLock::new();
    MAPS.get_or_init(|| {
        serde_json::from_str(include_str!("../python_mime_types.json"))
            .expect("bundled Python MIME map is valid")
    })
}
fn splitext(path: &str) -> (&str, &str) {
    let separator = path.rfind('/').map(|n| n + 1).unwrap_or(0);
    if let Some(dot) = path.rfind('.') {
        if dot >= separator && path[separator..dot].bytes().any(|b| b != b'.') {
            return (&path[..dot], &path[dot..]);
        }
    }
    (path, "")
}
fn url_path(input: &str) -> Option<(String, String)> {
    let normalized = input
        .trim_start_matches(|c: char| (c as u32) <= 32)
        .replace(['\t', '\r', '\n'], "");
    let at = normalized.find(':')?;
    let scheme = &normalized[..at];
    if scheme.len() <= 1
        || !scheme
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
        || !scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    let mut rest = &normalized[at + 1..];
    if let Some(authority) = rest.strip_prefix("//") {
        let at = authority.find(['/', '?', '#']).unwrap_or(authority.len());
        rest = &authority[at..];
    }
    rest = rest.split('#').next().unwrap_or("");
    rest = rest.split('?').next().unwrap_or("");
    const PARAM_SCHEMES: &[&str] = &[
        "ftp", "hdl", "prospero", "http", "imap", "https", "shttp", "rtsp", "rtsps", "rtspu",
        "sip", "sips", "mms", "sftp", "tel",
    ];
    if PARAM_SCHEMES.contains(&scheme.as_str()) {
        let start = rest.rfind('/').map(|n| n + 1).unwrap_or(0);
        if let Some(semicolon) = rest[start..].find(';') {
            rest = &rest[..start + semicolon];
        }
    }
    Some((scheme, rest.to_string()))
}
pub fn guess_type(input: &str) -> (Option<String>, Option<String>) {
    let path = if let Some((scheme, path)) = url_path(input) {
        if scheme == "data" {
            let Some(comma) = path.find(',') else {
                return (None, None);
            };
            let end = path[..comma].find(';').unwrap_or(comma);
            let media = &path[..end];
            return (
                Some(if media.contains('=') || !media.contains('/') {
                    "text/plain".to_string()
                } else {
                    media.to_string()
                }),
                None,
            );
        }
        path
    } else {
        input.to_string()
    };
    let (base, ext) = splitext(&path);
    guess_expanded(base.to_string(), ext.to_string(), maps())
}
fn guess_expanded(
    mut base: String,
    mut ext: String,
    maps: &Maps,
) -> (Option<String>, Option<String>) {
    let mut count = 0;
    while let Some(replacement) = maps.suffix_map.get(&ext.to_lowercase()) {
        let expanded = format!("{base}{replacement}");
        let (b, e) = splitext(&expanded);
        base = b.to_string();
        ext = e.to_string();
        count += 1;
        assert!(count < 16, "bundled MIME aliases must not cycle");
    }
    let encoding = maps.encodings_map.get(&ext).cloned();
    if encoding.is_some() {
        let (_, next) = splitext(&base);
        ext = next.to_string();
    }
    (maps.types_map.get(&ext.to_lowercase()).cloned(), encoding)
}
pub fn mime(input: &str) -> String {
    guess_type(input)
        .0
        .unwrap_or_else(|| "application/octet-stream".into())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn production_python_differential_corpus() {
        let fixtures: serde_json::Value =
            serde_json::from_str(include_str!("mime_cases.json")).unwrap();
        for case in fixtures.as_array().unwrap() {
            let input = case["input"].as_str().unwrap();
            let got = guess_type(input);
            assert_eq!(
                serde_json::json!([got.0, got.1]),
                case["expected"],
                "{input:?}"
            );
        }
    }
    #[test]
    fn file_urls_and_plain_filenames_have_distinct_query_handling() {
        assert_eq!(mime("file:///folder/a.ico?version=1"), mime("folder/a.ico"));
        assert_eq!(mime("folder/a.ico?version=1"), "application/octet-stream");
        assert_eq!(
            mime("https://example.test/a%2Eico"),
            "application/octet-stream"
        );
        assert_eq!(guess_type("a.TGZ").1.as_deref(), Some("gzip"));
        assert_eq!(guess_type("a.tar.GZ").1, None);
    }
}

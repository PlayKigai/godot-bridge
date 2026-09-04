use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

const ENCODED: &[u8] = b" \"<>`#?{}/%\\";
const HEX: &[u8; 16] = b"0123456789ABCDEF";

pub fn path_to_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    let mut segments = 0;
    for component in path.components() {
        if component == Component::RootDir {
            continue;
        }
        segments += 1;
        uri.push('/');
        push_encoded(component.as_os_str().as_bytes(), &mut uri);
    }
    if segments == 0 {
        uri.push('/');
    }
    uri
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let (scheme, mut rest) = uri.split_at_checked(5)?;
    if !scheme.eq_ignore_ascii_case("file:") {
        return None;
    }
    if let Some(authority) = rest.strip_prefix("//") {
        let split = authority.find('/').unwrap_or(authority.len());
        let host = &authority[..split];
        if !host.is_empty() && !host.eq_ignore_ascii_case("localhost") {
            return None;
        }
        rest = &authority[split..];
    }
    let end = rest.find(['?', '#']).unwrap_or(rest.len());
    let decoded = decode(&rest.as_bytes()[..end]);
    if !decoded.starts_with(b"/") {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(decoded)))
}

fn push_encoded(bytes: &[u8], uri: &mut String) {
    for byte in bytes {
        if byte.is_ascii() && !byte.is_ascii_control() && !ENCODED.contains(byte) {
            uri.push(char::from(*byte));
        } else {
            uri.push('%');
            uri.push(char::from(HEX[usize::from(byte >> 4)]));
            uri.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
}

fn decode(bytes: &[u8]) -> Vec<u8> {
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match hex_byte(bytes, index) {
            Some(byte) => {
                decoded.push(byte);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }
    decoded
}

fn hex_byte(bytes: &[u8], index: usize) -> Option<u8> {
    if bytes[index] != b'%' {
        return None;
    }
    let pair = bytes.get(index + 1..index + 3)?;
    let high = char::from(pair[0]).to_digit(16)?;
    let low = char::from(pair[1]).to_digit(16)?;
    Some((high * 16 + low) as u8)
}

#[cfg(test)]
mod tests {
    use super::{path_to_uri, uri_to_path};
    use std::path::{Path, PathBuf};

    const CAPTURED: [(&str, &str); 10] = [
        ("/", "file:///"),
        (
            "/home/dig/projects/zed-godot",
            "file:///home/dig/projects/zed-godot",
        ),
        (
            "/home/dig/my project/main.gd",
            "file:///home/dig/my%20project/main.gd",
        ),
        (
            "/tmp/scène/héllo wörld.gd",
            "file:///tmp/sc%C3%A8ne/h%C3%A9llo%20w%C3%B6rld.gd",
        ),
        (
            "/tmp/日本語/スクリプト.gd",
            "file:///tmp/%E6%97%A5%E6%9C%AC%E8%AA%9E/%E3%82%B9%E3%82%AF%E3%83%AA%E3%83%97%E3%83%88.gd",
        ),
        (
            "/tmp/a#b?c{d}e[f]g,h;i:j@k&l=m+n$o!p~q'r(s)t*u",
            "file:///tmp/a%23b%3Fc%7Bd%7De[f]g,h;i:j@k&l=m+n$o!p~q'r(s)t*u",
        ),
        (
            "/tmp/100% sure/back\\slash/quote\"dq/tick`bt/lt<gt>",
            "file:///tmp/100%25%20sure/back%5Cslash/quote%22dq/tick%60bt/lt%3Cgt%3E",
        ),
        ("/tmp/emoji 😀/x.gd", "file:///tmp/emoji%20%F0%9F%98%80/x.gd"),
        (
            "/tmp/tab\there/newline\nthere",
            "file:///tmp/tab%09here/newline%0Athere",
        ),
        ("/tmp/.hidden/../up/./down", "file:///tmp/.hidden/../up/down"),
    ];

    #[test]
    fn encoding_matches_urls_captured_from_the_url_crate() {
        for (path, uri) in CAPTURED {
            assert_eq!(path_to_uri(Path::new(path)), uri, "encoding {path}");
        }
    }

    #[test]
    fn decoding_returns_the_original_path() {
        for (path, uri) in CAPTURED {
            let expected = Path::new(path)
                .components()
                .filter(|component| *component != std::path::Component::CurDir)
                .collect::<PathBuf>();
            assert_eq!(uri_to_path(uri), Some(expected), "decoding {uri}");
        }
    }

    #[test]
    fn accepts_the_forms_zed_and_godot_send() {
        assert_eq!(
            uri_to_path("FILE:///tmp/a.gd"),
            Some(PathBuf::from("/tmp/a.gd"))
        );
        assert_eq!(
            uri_to_path("file://localhost/tmp/a.gd"),
            Some(PathBuf::from("/tmp/a.gd"))
        );
        assert_eq!(
            uri_to_path("file:/tmp/a.gd"),
            Some(PathBuf::from("/tmp/a.gd"))
        );
        assert_eq!(
            uri_to_path("file:///tmp/a.gd?v=1#top"),
            Some(PathBuf::from("/tmp/a.gd"))
        );
        assert_eq!(
            uri_to_path("file:///tmp/%zz%2/a.gd"),
            Some(PathBuf::from("/tmp/%zz%2/a.gd"))
        );
        assert_eq!(uri_to_path("file://elsewhere/tmp/a.gd"), None);
        assert_eq!(uri_to_path("https://example.com/a.gd"), None);
        assert_eq!(uri_to_path("file"), None);
    }

    #[test]
    fn rejects_paths_that_are_not_absolute() {
        assert_eq!(uri_to_path("file:"), None);
        assert_eq!(uri_to_path("file:x/../../etc"), None);
    }

    #[test]
    fn decodes_bytes_that_are_not_utf8() {
        use std::os::unix::ffi::OsStrExt;
        let path = uri_to_path("file:///tmp/%ff.gd").expect("invalid UTF-8 bytes decode");
        assert_eq!(path.as_os_str().as_bytes(), b"/tmp/\xff.gd");
    }
}

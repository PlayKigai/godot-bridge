//! `file:` URI encoding and decoding, in the dialect editors and Godot exchange.
//!
//! Unix paths are arbitrary bytes, so the Unix implementation never goes
//! through `str`. Windows paths are UTF-16 that the bridge handles as UTF-8,
//! and carry a drive letter that Godot always sends upper-cased.

use std::path::{Component, Path, PathBuf};

const ENCODED: &[u8] = b" \"<>`#?{}/%\\";

#[cfg(unix)]
pub fn path_to_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
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

#[cfg(unix)]
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let decoded = decode(strip_scheme_and_authority(uri)?);
    if !decoded.starts_with(b"/") {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(decoded)))
}

/// Encode an absolute Windows path. A drive letter becomes `/C:` after the
/// authority, which is the form Godot emits and editors accept.
#[cfg(windows)]
pub fn path_to_uri(path: &Path) -> String {
    use std::path::Prefix;
    let mut uri = String::from("file://");
    let mut segments = 0;
    let mut prefix_only = false;
    for component in path.components() {
        match component {
            // The prefix already carries the separator that follows it.
            Component::RootDir => continue,
            Component::Prefix(prefix) => {
                segments += 1;
                prefix_only = segments == 1;
                uri.push('/');
                match prefix.kind() {
                    Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                        uri.push(char::from(letter.to_ascii_uppercase()));
                        uri.push(':');
                    }
                    // A UNC or device path has no `file:` spelling the editors
                    // agree on; encode it so the round trip fails loudly
                    // rather than naming the wrong file.
                    _ => push_encoded(prefix.as_os_str().to_string_lossy().as_bytes(), &mut uri),
                }
            }
            component => {
                segments += 1;
                prefix_only = false;
                uri.push('/');
                push_encoded(component.as_os_str().to_string_lossy().as_bytes(), &mut uri);
            }
        }
    }
    if segments == 0 || prefix_only {
        uri.push('/');
    }
    uri
}

/// Decode a `file:` URI into an absolute Windows path. `file:///C:/x`,
/// `file:///c:/x` and `file:///c%3A/x` all yield `C:\x`; a UNC URI such as
/// `file://server/share` is refused.
#[cfg(windows)]
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let decoded = String::from_utf8(decode(strip_scheme_and_authority(uri)?)).ok()?;
    let rest = decoded.strip_prefix('/')?;
    let mut characters = rest.chars();
    let letter = characters.next()?;
    if !letter.is_ascii_alphabetic() || characters.next() != Some(':') {
        return None;
    }
    let tail = &rest[2..];
    if !tail.is_empty() && !tail.starts_with(['/', '\\']) {
        return None;
    }
    let mut path = String::with_capacity(rest.len() + 1);
    path.push(letter.to_ascii_uppercase());
    path.push(':');
    if tail.is_empty() {
        path.push('\\');
    }
    for character in tail.chars() {
        path.push(if character == '/' { '\\' } else { character });
    }
    Some(PathBuf::from(path))
}

fn strip_scheme_and_authority(uri: &str) -> Option<&[u8]> {
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
    Some(&rest.as_bytes()[..end])
}

fn push_encoded(bytes: &[u8], uri: &mut String) {
    use std::fmt::Write;
    for byte in bytes {
        if byte.is_ascii() && !byte.is_ascii_control() && !ENCODED.contains(byte) {
            uri.push(char::from(*byte));
        } else {
            let _ = write!(uri, "%{byte:02X}");
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

#[cfg(all(test, unix))]
mod unix_tests {
    use super::{path_to_uri, uri_to_path};
    use std::path::{Path, PathBuf};

    const CAPTURED: [(&str, &str); 10] = [
        ("/", "file:///"),
        (
            "/home/user/projects/godot-bridge",
            "file:///home/user/projects/godot-bridge",
        ),
        (
            "/home/user/my project/main.gd",
            "file:///home/user/my%20project/main.gd",
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
    fn accepts_the_forms_clients_and_godot_send() {
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

#[cfg(all(test, windows))]
mod windows_tests {
    use super::{path_to_uri, uri_to_path};
    use std::path::{Path, PathBuf};

    const CAPTURED: [(&str, &str); 8] = [
        (r"C:\", "file:///C:/"),
        (
            r"C:\Users\me\proj\main.gd",
            "file:///C:/Users/me/proj/main.gd",
        ),
        (
            r"C:\Users\me\my project\main.gd",
            "file:///C:/Users/me/my%20project/main.gd",
        ),
        (
            r"D:\tmp\scène\héllo wörld.gd",
            "file:///D:/tmp/sc%C3%A8ne/h%C3%A9llo%20w%C3%B6rld.gd",
        ),
        (
            r"C:\tmp\日本語\スクリプト.gd",
            "file:///C:/tmp/%E6%97%A5%E6%9C%AC%E8%AA%9E/%E3%82%B9%E3%82%AF%E3%83%AA%E3%83%97%E3%83%88.gd",
        ),
        (
            r"C:\tmp\emoji 😀\x.gd",
            "file:///C:/tmp/emoji%20%F0%9F%98%80/x.gd",
        ),
        (
            r#"C:\tmp\100% sure\quote"dq\tick`bt\lt<gt>"#,
            "file:///C:/tmp/100%25%20sure/quote%22dq/tick%60bt/lt%3Cgt%3E",
        ),
        (
            r"C:\tmp\a#b?c{d}e[f]g,h;i@k&l=m+n$o!p~q'r(s)t*u",
            "file:///C:/tmp/a%23b%3Fc%7Bd%7De[f]g,h;i@k&l=m+n$o!p~q'r(s)t*u",
        ),
    ];

    #[test]
    fn encoding_round_trips_drive_letter_paths() {
        for (path, uri) in CAPTURED {
            assert_eq!(path_to_uri(Path::new(path)), uri, "encoding {path}");
            assert_eq!(
                uri_to_path(uri),
                Some(PathBuf::from(path)),
                "decoding {uri}"
            );
        }
    }

    #[test]
    fn accepts_the_forms_clients_and_godot_send() {
        let expected = Some(PathBuf::from(r"C:\Users\me\proj\main.gd"));
        assert_eq!(uri_to_path("file:///C:/Users/me/proj/main.gd"), expected);
        assert_eq!(uri_to_path("file:///c:/Users/me/proj/main.gd"), expected);
        assert_eq!(uri_to_path("file:///c%3A/Users/me/proj/main.gd"), expected);
        assert_eq!(uri_to_path("file:///C%3a/Users/me/proj/main.gd"), expected);
        assert_eq!(uri_to_path(r"file:///C:\Users\me\proj\main.gd"), expected);
        assert_eq!(uri_to_path("FILE:///C:/Users/me/proj/main.gd"), expected);
        assert_eq!(
            uri_to_path("file://localhost/C:/Users/me/proj/main.gd"),
            expected
        );
        assert_eq!(uri_to_path("file:/C:/Users/me/proj/main.gd"), expected);
        assert_eq!(
            uri_to_path("file:///C:/Users/me/proj/main.gd?v=1#top"),
            expected
        );
    }

    #[test]
    fn refuses_unc_and_driveless_paths() {
        assert_eq!(uri_to_path("file://server/share/main.gd"), None);
        assert_eq!(uri_to_path("file:///tmp/a.gd"), None);
        assert_eq!(uri_to_path("file:///CC:/a.gd"), None);
        assert_eq!(uri_to_path("file:///C:relative/a.gd"), None);
        assert_eq!(uri_to_path("file:"), None);
        assert_eq!(uri_to_path("https://example.com/a.gd"), None);
        assert_eq!(uri_to_path("file"), None);
    }
}

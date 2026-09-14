use std::borrow::Cow;
use std::path::{Component, Path};

#[derive(Clone, Default)]
pub struct ScanFilter {
    pub diagnose_addons: bool,
    pub exclude: Exclude,
}

impl ScanFilter {
    pub fn new(diagnose_addons: bool, exclude: &[String]) -> Self {
        Self {
            diagnose_addons,
            exclude: Exclude::new(exclude),
        }
    }
}

#[derive(Clone, Default)]
pub struct Exclude {
    patterns: Vec<Pattern>,
}

#[derive(Clone)]
struct Pattern {
    segments: Vec<Segment>,
    directory_only: bool,
}

#[derive(Clone)]
enum Segment {
    Any,
    Name(String),
}

impl Exclude {
    pub fn new(patterns: &[String]) -> Self {
        Self {
            patterns: patterns
                .iter()
                .filter_map(|pattern| Pattern::compile(pattern))
                .collect(),
        }
    }

    pub fn is_excluded(&self, relative: &Path, is_dir: bool) -> bool {
        if self.patterns.is_empty() {
            return false;
        }
        let components = relative
            .components()
            .filter_map(|component| match component {
                Component::Normal(name) => Some(name.to_string_lossy()),
                _ => None,
            })
            .collect::<Vec<_>>();
        if components.is_empty() {
            return false;
        }
        let mut reachable = vec![false; components.len() + 1];
        self.patterns
            .iter()
            .any(|pattern| pattern.matches(&components, is_dir, &mut reachable))
    }
}

impl Pattern {
    fn compile(raw: &str) -> Option<Self> {
        let normalized = raw.replace('\\', "/");
        let rooted = normalized.starts_with('/') || normalized.starts_with("./");
        let trimmed = normalized
            .strip_prefix("./")
            .unwrap_or(&normalized)
            .trim_start_matches('/');
        let directory_only = trimmed.ends_with('/');
        let body = trimmed.strip_suffix('/').unwrap_or(trimmed);
        let mut segments = body
            .split('/')
            .filter(|segment| !segment.is_empty())
            .map(|segment| match segment {
                "**" => Segment::Any,
                name => Segment::Name(name.to_owned()),
            })
            .collect::<Vec<_>>();
        if segments.is_empty() {
            return None;
        }
        if !rooted && segments.len() == 1 {
            segments.insert(0, Segment::Any);
        }
        if segments.len() > 1 && matches!(segments.last(), Some(Segment::Any)) {
            // A trailing /** matches everything inside, not the directory itself.
            segments.push(Segment::Name("*".to_owned()));
        }
        Some(Self {
            segments,
            directory_only,
        })
    }

    fn matches(&self, components: &[Cow<'_, str>], is_dir: bool, reachable: &mut [bool]) -> bool {
        reachable.fill(false);
        reachable[0] = true;
        for segment in &self.segments {
            match segment {
                Segment::Any => {
                    let mut any = false;
                    for slot in reachable.iter_mut() {
                        any |= *slot;
                        *slot = any;
                    }
                }
                Segment::Name(glob) => {
                    for (index, component) in components.iter().enumerate().rev() {
                        reachable[index + 1] =
                            reachable[index] && glob_matches(glob.as_bytes(), component.as_bytes());
                    }
                    reachable[0] = false;
                }
            }
            if !reachable.contains(&true) {
                return false;
            }
        }
        let whole = components.len();
        (1..whole).any(|end| reachable[end])
            || (reachable[whole] && (is_dir || !self.directory_only))
    }
}

fn char_len(lead: u8) -> usize {
    match lead {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

fn glob_matches(pattern: &[u8], name: &[u8]) -> bool {
    let (mut pattern_index, mut name_index) = (0, 0);
    let mut star = None;
    let mut star_name_index = 0;
    while name_index < name.len() {
        if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star = Some(pattern_index);
            star_name_index = name_index;
            pattern_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'?' {
            pattern_index += 1;
            name_index += char_len(name[name_index]);
        } else if pattern_index < pattern.len() && pattern[pattern_index] == name[name_index] {
            pattern_index += 1;
            name_index += 1;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            star_name_index += char_len(name[star_name_index]);
            name_index = star_name_index;
        } else {
            return false;
        }
    }
    pattern[pattern_index..].iter().all(|byte| *byte == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exclude(patterns: &[&str]) -> Exclude {
        Exclude::new(
            &patterns
                .iter()
                .map(|pattern| pattern.to_string())
                .collect::<Vec<_>>(),
        )
    }

    fn excluded(exclude: &Exclude, path: &str, is_dir: bool) -> bool {
        exclude.is_excluded(Path::new(path), is_dir)
    }

    #[test]
    fn empty_and_separator_only_patterns_are_ignored() {
        let patterns = exclude(&["", "/", "./", "//"]);
        assert!(!excluded(&patterns, "a.gd", false));
        assert!(!excluded(&patterns, "a", true));
    }

    #[test]
    fn repeated_separators_collapse() {
        let patterns = exclude(&["src//gen", "//tests", "build//"]);
        assert!(excluded(&patterns, "src/gen", true));
        assert!(excluded(&patterns, "tests", true));
        assert!(!excluded(&patterns, "a/tests", true));
        assert!(excluded(&patterns, "a/build", true));
        assert!(!excluded(&patterns, "a/build", false));
    }

    #[test]
    fn star_does_not_cross_directory_separators() {
        let patterns = exclude(&["src/*.gd"]);
        assert!(excluded(&patterns, "src/a.gd", false));
        assert!(!excluded(&patterns, "src/sub/a.gd", false));
    }

    #[test]
    fn question_matches_one_character_but_not_a_separator() {
        let patterns = exclude(&["a?.gd"]);
        assert!(excluded(&patterns, "ab.gd", false));
        assert!(excluded(&patterns, "aé.gd", false));
        assert!(!excluded(&patterns, "a.gd", false));
        assert!(!excluded(&patterns, "abc.gd", false));
        assert!(!excluded(&exclude(&["a?b"]), "a/b", false));
    }

    #[test]
    fn star_matches_multibyte_names() {
        assert!(excluded(&exclude(&["*é.gd"]), "café.gd", false));
        assert!(!excluded(&exclude(&["caf?.gd"]), "cafés.gd", false));
    }

    #[test]
    fn double_star_matches_zero_or_more_components() {
        let patterns = exclude(&["src/**/gen"]);
        assert!(excluded(&patterns, "src/gen", true));
        assert!(excluded(&patterns, "src/a/b/gen", true));
        assert!(!excluded(&patterns, "other/gen", true));
    }

    #[test]
    fn double_star_prefix_matches_at_the_root() {
        let patterns = exclude(&["**/tests"]);
        assert!(excluded(&patterns, "tests", true));
        assert!(excluded(&patterns, "a/b/tests", true));
        assert!(!excluded(&patterns, "a/b/other", true));
    }

    #[test]
    fn double_star_suffix_does_not_match_the_directory_itself() {
        let patterns = exclude(&["src/**", "gen/**//"]);
        assert!(!excluded(&patterns, "src", true));
        assert!(!excluded(&patterns, "gen", true));
        assert!(excluded(&patterns, "gen/a", true));
        assert!(excluded(&patterns, "src/a.gd", false));
        assert!(excluded(&patterns, "src/sub", true));
        assert!(excluded(&patterns, "src/sub/deep/a.gd", false));
    }

    #[test]
    fn bare_pattern_matches_every_path_component() {
        let patterns = exclude(&["tests", "*.test.gd"]);
        assert!(excluded(&patterns, "tests", true));
        assert!(excluded(&patterns, "a/tests", true));
        assert!(excluded(&patterns, "a/tests/b.gd", false));
        assert!(excluded(&patterns, "main.test.gd", false));
        assert!(excluded(&patterns, "a/deep/main.test.gd", false));
        assert!(!excluded(&patterns, "a/main.gd", false));
    }

    #[test]
    fn pattern_with_slash_is_anchored_at_the_root() {
        let patterns = exclude(&["levels/generated"]);
        assert!(excluded(&patterns, "levels/generated", true));
        assert!(excluded(&patterns, "levels/generated/a.gd", false));
        assert!(!excluded(&patterns, "other/levels/generated", true));
    }

    #[test]
    fn trailing_slash_matches_directories_only() {
        let patterns = exclude(&["build/"]);
        assert!(excluded(&patterns, "build", true));
        assert!(!excluded(&patterns, "build", false));
        assert!(excluded(&patterns, "build/a.gd", false));

        let patterns = exclude(&["src/gen/"]);
        assert!(excluded(&patterns, "src/gen", true));
        assert!(excluded(&patterns, "src/gen/a.gd", false));
        assert!(!excluded(&patterns, "src/gen", false));
    }

    #[test]
    fn leading_slash_and_dot_slash_anchor() {
        let patterns = exclude(&["/tests", "./build/", ".godot"]);
        assert!(excluded(&patterns, "tests", true));
        assert!(!excluded(&patterns, "a/tests", true));
        assert!(excluded(&patterns, "build", true));
        assert!(!excluded(&patterns, "a/build", true));
        assert!(excluded(&patterns, "build/a.gd", false));
        assert!(excluded(&patterns, "a/.godot", true));
    }

    #[test]
    fn double_star_alone_matches_every_path() {
        let patterns = exclude(&["**"]);
        assert!(excluded(&patterns, "a", true));
        assert!(excluded(&patterns, "a/b.gd", false));
    }

    #[test]
    fn project_root_is_never_excluded() {
        let patterns = exclude(&["**", "tests"]);
        assert!(!excluded(&patterns, "", true));
    }

    #[test]
    fn backslashes_in_patterns_are_separators() {
        let patterns = exclude(&[r"sub\scripts"]);
        assert!(excluded(&patterns, "sub/scripts", true));
        assert!(excluded(&patterns, "sub/scripts/a.gd", false));
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_are_normalised() {
        let patterns = exclude(&["sub/scripts", "*.test.gd"]);
        assert!(excluded(&patterns, r"sub\scripts\a.gd", false));
        assert!(excluded(&patterns, r"a\main.test.gd", false));
    }

    #[cfg(unix)]
    #[test]
    fn unix_backslash_is_a_name_character() {
        assert!(!excluded(&exclude(&["b.gd"]), r"a\b.gd", false));
    }
}

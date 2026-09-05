use crate::json::Value;
use std::cmp::Ordering;

#[derive(Clone, Debug, PartialEq)]
pub struct Symbol {
    pub name: String,
    folded_name: String,
    folded_container_name: String,
    boundary_positions: BoundaryPositions,
    container_boundary_positions: BoundaryPositions,
    pub kind: u8,
    pub container: String,
    pub range: [u32; 4],
}

#[derive(Clone, Debug, PartialEq)]
enum BoundaryPositions {
    Mask(u64),
    List(Vec<u8>),
}

impl BoundaryPositions {
    fn contains(&self, position: usize) -> bool {
        match self {
            Self::Mask(mask) => position < 64 && mask & (1 << position) != 0,
            Self::List(positions) => u8::try_from(position)
                .ok()
                .is_some_and(|position| positions.binary_search(&position).is_ok()),
        }
    }
}

#[derive(Debug)]
struct Match<'a> {
    symbol: &'a Symbol,
    uri: &'a str,
    gap: usize,
    offset: usize,
    indices: Vec<usize>,
    range_start: (u32, u32),
    boundary_count: usize,
}

pub fn flatten(result: &Value, default_uri: &str) -> Vec<(String, Symbol)> {
    let Some(items) = result.as_array() else {
        return Vec::new();
    };
    let default_uri = normalize_uri(default_uri);
    let mut output = Vec::new();
    for item in items {
        if let Some(location) = item.get("location") {
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let folded_name = name.to_lowercase();
            let container = item
                .get("containerName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let container_name = if container.is_empty() {
                name.clone()
            } else {
                format!("{container}.{name}")
            };
            let uri = location
                .get("uri")
                .and_then(Value::as_str)
                .map(normalize_uri)
                .unwrap_or_else(|| default_uri.clone());
            output.push((
                uri,
                Symbol {
                    boundary_positions: boundary_positions(&name),
                    container_boundary_positions: boundary_positions(&container_name),
                    folded_container_name: container_name.to_lowercase(),
                    name,
                    folded_name,
                    kind: item
                        .get("kind")
                        .and_then(Value::as_u64)
                        .and_then(|kind| u8::try_from(kind).ok())
                        .unwrap_or_default(),
                    container,
                    range: range_values(location.get("range").unwrap_or(&Value::Null)),
                },
            ));
        } else {
            flatten_document(item, &default_uri, "", &mut output);
        }
    }
    output
}

fn flatten_document(item: &Value, uri: &str, container: &str, output: &mut Vec<(String, Symbol)>) {
    let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
    let uri = item
        .get("uri")
        .and_then(Value::as_str)
        .map(normalize_uri)
        .unwrap_or_else(|| uri.to_owned());
    let folded_name = name.to_lowercase();
    let current_container = container.to_owned();
    let container_name = if current_container.is_empty() {
        name.to_owned()
    } else {
        format!("{current_container}.{name}")
    };
    output.push((
        uri.clone(),
        Symbol {
            name: name.to_owned(),
            folded_name,
            folded_container_name: container_name.to_lowercase(),
            boundary_positions: boundary_positions(name),
            container_boundary_positions: boundary_positions(&container_name),
            kind: item
                .get("kind")
                .and_then(Value::as_u64)
                .and_then(|kind| u8::try_from(kind).ok())
                .unwrap_or_default(),
            container: current_container,
            range: item
                .get("selectionRange")
                .or_else(|| item.get("range"))
                .map_or([0; 4], range_values),
        },
    ));
    if let Some(children) = item.get("children").and_then(Value::as_array) {
        for child in children {
            flatten_document(child, &uri, &container_name, output);
        }
    }
}

fn normalize_uri(uri: &str) -> String {
    if uri.starts_with("file:") {
        crate::root::path_to_uri(&crate::root::doc_key(uri))
    } else {
        uri.to_owned()
    }
}

fn boundary_positions(value: &str) -> BoundaryPositions {
    if value.is_ascii() && value.len() <= 64 {
        let mut mask = 0;
        let bytes = value.as_bytes();
        for (index, &character) in bytes.iter().enumerate() {
            if index == 0
                || bytes[index - 1] == b'_'
                || (character.is_ascii_uppercase() && bytes[index - 1].is_ascii_lowercase())
            {
                mask |= 1 << index;
            }
        }
        BoundaryPositions::Mask(mask)
    } else {
        let characters = value.chars().collect::<Vec<_>>();
        BoundaryPositions::List(
            characters
                .iter()
                .enumerate()
                .filter_map(|(index, character)| {
                    (index == 0
                        || characters[index - 1] == '_'
                        || (character.is_uppercase() && characters[index - 1].is_lowercase()))
                    .then(|| u8::try_from(index).ok())
                    .flatten()
                })
                .collect(),
        )
    }
}

pub fn search<'a>(symbols: impl IntoIterator<Item = &'a Symbol>, query: &str) -> Vec<Symbol> {
    search_with_uris(symbols.into_iter().map(|symbol| ("", symbol)), query)
        .into_iter()
        .map(|(_, symbol)| symbol.clone())
        .collect()
}

pub fn search_with_uris<'a>(
    symbols: impl IntoIterator<Item = (&'a str, &'a Symbol)>,
    query: &str,
) -> Vec<(&'a str, &'a Symbol)> {
    let container_query = query.contains('.') || query.contains(' ');
    let query = query.to_lowercase();
    let query = if container_query {
        query.replace(' ', ".")
    } else {
        query
    };
    let mut scratch = Vec::new();
    let mut candidate = Vec::new();
    let mut matches = symbols
        .into_iter()
        .filter_map(|(uri, symbol)| {
            let target = if container_query {
                &symbol.folded_container_name
            } else {
                &symbol.folded_name
            };
            if target.chars().count() > 256 {
                return None;
            }
            let (gap, offset, indices) =
                best_match_into(target, &query, &mut scratch, &mut candidate)?;
            let boundary_positions = if container_query {
                &symbol.container_boundary_positions
            } else {
                &symbol.boundary_positions
            };
            let boundary_count = indices
                .iter()
                .filter(|index| boundary_positions.contains(**index))
                .count();
            Some(Match {
                symbol,
                uri,
                gap: gap.saturating_sub(boundary_count * 4),
                offset,
                boundary_count,
                indices,
                range_start: (symbol.range[0], symbol.range[1]),
            })
        })
        .collect::<Vec<_>>();
    if matches.len() > 200 {
        matches.select_nth_unstable_by(199, compare_matches);
        matches.truncate(200);
    }
    matches.sort_by(compare_matches);
    matches
        .into_iter()
        .map(|matched| (matched.uri, matched.symbol))
        .collect()
}

fn compare_matches(left: &Match<'_>, right: &Match<'_>) -> Ordering {
    left.gap
        .cmp(&right.gap)
        .then(right.boundary_count.cmp(&left.boundary_count))
        .then(left.offset.cmp(&right.offset))
        .then(left.symbol.name.cmp(&right.symbol.name))
        .then(left.uri.cmp(right.uri))
        .then(left.range_start.cmp(&right.range_start))
        .then(left.indices.cmp(&right.indices))
}

fn best_match_into(
    name: &str,
    query: &str,
    indices: &mut Vec<usize>,
    candidate: &mut Vec<usize>,
) -> Option<(usize, usize, Vec<usize>)> {
    if name.is_ascii() && query.is_ascii() {
        let (gap, offset) =
            best_match_ascii(name.as_bytes(), query.as_bytes(), indices, candidate)?;
        return Some((gap, offset, indices.clone()));
    }
    best_match_unicode(name, query)
}

fn best_match_ascii(
    name: &[u8],
    query: &[u8],
    indices: &mut Vec<usize>,
    candidate: &mut Vec<usize>,
) -> Option<(usize, usize)> {
    if query.is_empty() {
        indices.clear();
        return Some((0, 0));
    }
    if !is_subsequence_bytes(name, query) {
        return None;
    }
    let mut best = None;
    for start in 0..name.len() {
        if name[start] != query[0] {
            continue;
        }
        candidate.clear();
        candidate.push(start);
        let mut position = start + 1;
        for &wanted in &query[1..] {
            let Some(offset) = name[position..].iter().position(|&value| value == wanted) else {
                candidate.clear();
                break;
            };
            position += offset + 1;
            candidate.push(position - 1);
        }
        if candidate.len() != query.len() {
            continue;
        }
        let gap = candidate.last().unwrap() - start + 1 - query.len();
        if best.is_none_or(|(current_gap, current_start)| {
            (gap, start, candidate.as_slice()) < (current_gap, current_start, indices.as_slice())
        }) {
            indices.clear();
            indices.extend_from_slice(candidate);
            best = Some((gap, start));
        }
    }
    best
}

fn best_match_unicode(name: &str, query: &str) -> Option<(usize, usize, Vec<usize>)> {
    if query.is_empty() {
        return Some((0, 0, Vec::new()));
    }
    if !is_subsequence(name, query) {
        return None;
    }
    let name = name.chars().collect::<Vec<_>>();
    let query = query.chars().collect::<Vec<_>>();
    let mut next = vec![vec![name.len(); name.len() + 1]; query.len()];
    for query_index in (0..query.len()).rev() {
        let mut next_index = name.len();
        for name_index in (0..name.len()).rev() {
            if name[name_index] == query[query_index] {
                next_index = name_index;
            }
            next[query_index][name_index] = next_index;
        }
    }
    let mut best = None;
    for start in 0..name.len() {
        if name[start] != query[0] {
            continue;
        }
        let mut indices = vec![start];
        let mut position = start + 1;
        for row in next.iter().skip(1) {
            let index = row[position.min(name.len())];
            if index == name.len() {
                break;
            }
            indices.push(index);
            position = index + 1;
        }
        if indices.len() != query.len() {
            continue;
        }
        let gap = indices.last().unwrap() - start + 1 - query.len();
        let candidate = (gap, start, indices);
        if best.as_ref().is_none_or(|current| candidate < *current) {
            best = Some(candidate);
        }
    }
    best
}

fn is_subsequence_bytes(name: &[u8], query: &[u8]) -> bool {
    let mut query = query.iter();
    let Some(mut wanted) = query.next() else {
        return true;
    };
    for character in name {
        if character == wanted {
            let Some(next) = query.next() else {
                return true;
            };
            wanted = next;
        }
    }
    false
}

fn is_subsequence(name: &str, query: &str) -> bool {
    let mut query = query.chars();
    let Some(mut wanted) = query.next() else {
        return true;
    };
    for character in name.chars() {
        if character == wanted {
            let Some(next) = query.next() else {
                return true;
            };
            wanted = next;
        }
    }
    false
}

fn range_values(range: &Value) -> [u32; 4] {
    let start = range.get("start").unwrap_or(&Value::Null);
    let end = range.get("end").unwrap_or(&Value::Null);
    [
        range_value(start, "line"),
        range_value(start, "character"),
        range_value(end, "line"),
        range_value(end, "character"),
    ]
}

fn range_value(position: &Value, key: &str) -> u32 {
    position
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or_default()
}

pub fn symbol_information(symbol: &Symbol, uri: &str) -> Value {
    let [start_line, start_character, end_line, end_character] = symbol.range;
    crate::json!({"name": (symbol.name.clone()), "kind": (u64::from(symbol.kind)), "containerName": (symbol.container.clone()), "location": {"uri": (uri.to_owned()), "range": {"start": {"line": (u64::from(start_line)), "character": (u64::from(start_character))}, "end": {"line": (u64::from(end_line)), "character": (u64::from(end_character))}}}})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol(name: &str, _uri: &str, line: u64) -> Symbol {
        symbol_with_container(name, "", line)
    }

    fn symbol_with_container(name: &str, container: &str, line: u64) -> Symbol {
        let folded_name = name.to_lowercase();
        let container_name = if container.is_empty() {
            name.to_owned()
        } else {
            format!("{container}.{name}")
        };
        Symbol {
            name: name.to_owned(),
            folded_name,
            folded_container_name: container_name.to_lowercase(),
            boundary_positions: boundary_positions(name),
            container_boundary_positions: boundary_positions(&container_name),
            kind: 12,
            container: container.to_owned(),
            range: [line as u32, 0, line as u32, 1],
        }
    }

    #[test]
    fn ranking_and_empty_query_are_stable() {
        let items = vec![
            symbol("a_ready", "file:///b", 0),
            symbol("_ready", "file:///a", 1),
            symbol("read_y", "file:///a", 0),
        ];
        assert_eq!(
            search(&items, "ready")
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["read_y", "_ready", "a_ready"]
        );
        assert_eq!(
            search(&items, "")
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["_ready", "a_ready", "read_y"]
        );
    }

    #[test]
    fn alignment_uses_gap_then_offset_then_indices() {
        let items = vec![
            symbol("rxxeady", "file:///a", 0),
            symbol("ready", "file:///b", 0),
            symbol("xready", "file:///c", 0),
        ];
        assert_eq!(search(&items, "ready").first().unwrap().name, "ready");
        assert_eq!(
            best_match_unicode("rreaddy", "ready").unwrap().2,
            vec![1, 2, 3, 4, 6]
        );
    }

    #[test]
    fn boundary_bonus_prefers_word_starts() {
        let items = vec![
            symbol("improve", "file:///a", 0),
            symbol("player_velocity", "file:///b", 0),
        ];
        assert_eq!(search(&items, "pv")[0].name, "player_velocity");
    }

    #[test]
    fn container_queries_match_dotted_and_spaced_names() {
        let items = vec![symbol_with_container("jump", "Player", 0)];
        assert_eq!(search(&items, "player jump")[0].name, "jump");
        assert_eq!(search(&items, "Player.jump")[0].name, "jump");
        assert!(search(&items, "player").is_empty());
    }

    #[test]
    fn repeated_characters_match_quickly() {
        let items = vec![symbol(&"a".repeat(64), "file:///a", 0)];
        assert_eq!(search(&items, &"a".repeat(32)).len(), 1);
        assert!(search(&items, &format!("{}b", "a".repeat(32))).is_empty());
    }

    #[test]
    fn truncates_to_two_hundred() {
        let items = (0..201)
            .map(|index| symbol(&format!("x{index:03}"), "file:///a", index))
            .collect::<Vec<_>>();
        assert_eq!(search(&items, "x").len(), 200);
    }

    #[test]
    fn synthetic_search_benchmark_matches_reference() {
        let items = (0..32_000)
            .map(|index| symbol(&format!("symbol_{index:05}_ready"), "file:///a", index))
            .collect::<Vec<_>>();
        for query in ["ready", "", "zzz"] {
            let start = std::time::Instant::now();
            let actual = search(&items, query);
            let elapsed = start.elapsed();
            match query {
                "ready" => assert_eq!(actual.len(), 200),
                "" => {
                    assert_eq!(actual.len(), 200);
                    assert_eq!(actual[0].name, "symbol_00000_ready");
                }
                "zzz" => assert!(actual.is_empty()),
                _ => unreachable!(),
            }
            println!("search {query:?}: {elapsed:?}");
        }
    }

    #[test]
    fn flattens_nested_document_symbols() {
        let result = crate::json!([
            {"name":"Root","kind":5,"range":{"start":{"line":0},"end":{"line":4}},"selectionRange":{"start":{"line":1},"end":{"line":1}},"children":[
                {"name":"Child","kind":6,"range":{"start":{"line":2},"end":{"line":3}},"selectionRange":{"start":{"line":2},"end":{"line":2}}},
                {"name":"Other","kind":6,"range":{"start":{"line":3},"end":{"line":4}},"selectionRange":{"start":{"line":3},"end":{"line":3}},"uri":"file:///tmp/../tmp/other.gd","children":[{"name":"Nested","kind":6,"selectionRange":{"start":{"line":4},"end":{"line":4}}}]}
            ]}
        ]);
        let flattened = flatten(&result, "file:///tmp/../tmp/main.gd");
        assert_eq!(flattened.len(), 4);
        assert_eq!(flattened[0].1.container, "");
        assert_eq!(flattened[1].1.container, "Root");
        assert_eq!(flattened[0].1.range[0], 1);
        assert_eq!(flattened[0].0, "file:///tmp/main.gd");
        assert_eq!(flattened[1].0, "file:///tmp/main.gd");
        assert_eq!(flattened[2].0, "file:///tmp/other.gd");
        assert_eq!(flattened[3].0, "file:///tmp/other.gd");
    }
}

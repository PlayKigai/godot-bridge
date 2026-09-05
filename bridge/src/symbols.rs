use crate::json::Value;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::io::Write;

#[derive(Clone, Debug, PartialEq)]
pub struct Symbol {
    pub name: String,
    folded_name: Option<String>,
    folded_container_name: Option<String>,
    boundary_positions: BoundaryPositions,
    container_boundary_positions: BoundaryPositions,
    pub kind: u8,
    pub container: u32,
    pub range: [u32; 4],
}

#[derive(Clone, Debug, PartialEq)]
enum BoundaryPositions {
    Mask(u64),
    List(Box<[u64]>),
}

impl BoundaryPositions {
    fn contains(&self, position: usize) -> bool {
        match self {
            Self::Mask(mask) => position < 64 && mask & (1 << position) != 0,
            Self::List(positions) => u64::try_from(position)
                .ok()
                .is_some_and(|position| positions.binary_search(&position).is_ok()),
        }
    }
}

#[derive(Default)]
pub struct ContainerTable {
    names: Vec<String>,
    indices: HashMap<String, u32>,
}

impl ContainerTable {
    fn intern(&mut self, name: &str) -> u32 {
        if let Some(&index) = self.indices.get(name) {
            return index;
        }
        let index = self.names.len() as u32;
        self.names.push(name.to_owned());
        self.indices.insert(name.to_owned(), index);
        index
    }

    fn get(&self, index: u32) -> &str {
        self.names
            .get(index as usize)
            .map(String::as_str)
            .unwrap_or_default()
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

struct HeapMatch<'a>(Match<'a>);

impl PartialEq for HeapMatch<'_> {
    fn eq(&self, other: &Self) -> bool {
        compare_matches(&self.0, &other.0) == Ordering::Equal
    }
}

impl Eq for HeapMatch<'_> {}

impl PartialOrd for HeapMatch<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapMatch<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_matches(&self.0, &other.0)
    }
}

pub fn flatten(
    result: &Value,
    default_uri: &str,
    containers: &mut ContainerTable,
) -> Vec<(String, Symbol)> {
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
            let container = item
                .get("containerName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let container_index = containers.intern(&container);
            let uri = location
                .get("uri")
                .and_then(Value::as_str)
                .map(normalize_uri)
                .unwrap_or_else(|| default_uri.clone());
            let folded_name = (!name.is_ascii()).then(|| name.to_lowercase());
            let folded_container_name = (!container.is_ascii() || !name.is_ascii())
                .then(|| format!("{container}.{name}").to_lowercase());
            output.push((
                uri,
                Symbol {
                    boundary_positions: boundary_positions(&name),
                    container_boundary_positions: boundary_positions_parts(&container, &name),
                    folded_container_name,
                    name,
                    folded_name,
                    kind: item
                        .get("kind")
                        .and_then(Value::as_u64)
                        .and_then(|kind| u8::try_from(kind).ok())
                        .unwrap_or_default(),
                    container: container_index,
                    range: range_values(location.get("range").unwrap_or(&Value::Null)),
                },
            ));
        } else {
            flatten_document(item, &default_uri, "", &mut output, containers);
        }
    }
    output
}

fn flatten_document(
    item: &Value,
    uri: &str,
    container: &str,
    output: &mut Vec<(String, Symbol)>,
    containers: &mut ContainerTable,
) {
    let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
    let uri = item
        .get("uri")
        .and_then(Value::as_str)
        .map(normalize_uri)
        .unwrap_or_else(|| uri.to_owned());
    let container_name = if container.is_empty() {
        name.to_owned()
    } else {
        format!("{container}.{name}")
    };
    output.push((
        uri.clone(),
        Symbol {
            name: name.to_owned(),
            folded_name: (!name.is_ascii()).then(|| name.to_lowercase()),
            folded_container_name: (!container_name.is_ascii())
                .then(|| container_name.to_lowercase()),
            boundary_positions: boundary_positions(name),
            container_boundary_positions: boundary_positions_parts(container, name),
            kind: item
                .get("kind")
                .and_then(Value::as_u64)
                .and_then(|kind| u8::try_from(kind).ok())
                .unwrap_or_default(),
            container: containers.intern(container),
            range: item
                .get("selectionRange")
                .or_else(|| item.get("range"))
                .map_or([0; 4], range_values),
        },
    ));
    if let Some(children) = item.get("children").and_then(Value::as_array) {
        for child in children {
            flatten_document(child, &uri, &container_name, output, containers);
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
        let mut positions = Vec::new();
        let mut previous = None;
        for (index, character) in value.chars().enumerate() {
            if index == 0
                || previous == Some('_')
                || (character.is_uppercase() && previous.is_some_and(char::is_lowercase))
            {
                positions.push(index as u64);
            }
            previous = Some(character);
        }
        BoundaryPositions::List(positions.into_boxed_slice())
    }
}

fn boundary_positions_parts(container: &str, name: &str) -> BoundaryPositions {
    if container.is_empty() {
        return boundary_positions(name);
    }
    if container.is_ascii() && name.is_ascii() && container.len() + 1 + name.len() <= 64 {
        let mut mask = 0;
        let mut previous = None;
        for (index, &character) in container
            .as_bytes()
            .iter()
            .chain(std::iter::once(&b'.'))
            .chain(name.as_bytes())
            .enumerate()
        {
            if index == 0
                || previous == Some(b'_')
                || (character.is_ascii_uppercase()
                    && previous.is_some_and(|value| value.is_ascii_lowercase()))
            {
                mask |= 1 << index;
            }
            previous = Some(character);
        }
        return BoundaryPositions::Mask(mask);
    }
    let mut positions = Vec::new();
    let mut previous = None;
    for (index, character) in container
        .chars()
        .chain(std::iter::once('.'))
        .chain(name.chars())
        .enumerate()
    {
        if index == 0
            || previous == Some('_')
            || (character.is_uppercase() && previous.is_some_and(char::is_lowercase))
        {
            positions.push(index as u64);
        }
        previous = Some(character);
    }
    if positions.len() <= 64 {
        let mask = positions
            .into_iter()
            .fold(0, |mask, position| mask | (1 << position));
        BoundaryPositions::Mask(mask)
    } else {
        BoundaryPositions::List(positions.into_boxed_slice())
    }
}

#[cfg(test)]
fn search<'a>(symbols: impl IntoIterator<Item = &'a Symbol>, query: &str) -> Vec<Symbol> {
    let containers = ContainerTable::default();
    search_with_uris(
        symbols.into_iter().map(|symbol| ("", symbol)),
        query,
        &containers,
    )
    .into_iter()
    .map(|(_, symbol)| symbol.clone())
    .collect()
}

pub fn search_with_uris<'a>(
    symbols: impl IntoIterator<Item = (&'a str, &'a Symbol)>,
    query: &str,
    containers: &ContainerTable,
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
    let mut target = Vec::new();
    let mut matches = BinaryHeap::with_capacity(200);
    for (uri, symbol) in symbols {
        let Some((gap, offset)) = (if container_query {
            if let Some(target) = symbol.folded_container_name.as_deref() {
                if target.len() > 256 && target.chars().count() > 256 {
                    continue;
                }
                best_match_into(target, &query, &mut scratch, &mut candidate)
            } else {
                let container = containers.get(symbol.container).as_bytes();
                if container.is_empty() {
                    if symbol.name.len() > 256 {
                        continue;
                    }
                    best_match_into(&symbol.name, &query, &mut scratch, &mut candidate)
                } else {
                    if container.len() + 1 + symbol.name.len() > 256 {
                        continue;
                    }
                    target.clear();
                    target.extend_from_slice(container);
                    target.push(b'.');
                    target.extend_from_slice(symbol.name.as_bytes());
                    best_match_ascii(&target, query.as_bytes(), &mut scratch, &mut candidate)
                }
            }
        } else if let Some(target) = symbol.folded_name.as_deref() {
            if target.len() > 256 && target.chars().count() > 256 {
                continue;
            }
            best_match_into(target, &query, &mut scratch, &mut candidate)
        } else {
            if symbol.name.len() > 256 {
                continue;
            } else {
                best_match_into(&symbol.name, &query, &mut scratch, &mut candidate)
            }
        }) else {
            continue;
        };
        let boundary_positions = if container_query {
            &symbol.container_boundary_positions
        } else {
            &symbol.boundary_positions
        };
        let boundary_count = scratch
            .iter()
            .filter(|index| boundary_positions.contains(**index))
            .count();
        let matched = Match {
            symbol,
            uri,
            gap: gap.saturating_sub(boundary_count * 4),
            offset,
            boundary_count,
            indices: Vec::new(),
            range_start: (symbol.range[0], symbol.range[1]),
        };
        if matches.len() < 200 {
            matches.push(HeapMatch(Match {
                indices: scratch.clone(),
                ..matched
            }));
        } else {
            if matches.peek().is_some_and(|worst| {
                compare_match_parts(&matched, &scratch, &worst.0) == Ordering::Less
            }) {
                matches.pop();
                matches.push(HeapMatch(Match {
                    indices: scratch.clone(),
                    ..matched
                }));
            }
        }
    }
    let mut matches = matches.into_vec();
    matches.sort_by(|left, right| compare_matches(&left.0, &right.0));
    matches
        .into_iter()
        .map(|matched| (matched.0.uri, matched.0.symbol))
        .collect()
}

fn compare_matches(left: &Match<'_>, right: &Match<'_>) -> Ordering {
    compare_match_parts(left, &left.indices, right)
}

fn compare_match_parts(left: &Match<'_>, left_indices: &[usize], right: &Match<'_>) -> Ordering {
    left.gap
        .cmp(&right.gap)
        .then(right.boundary_count.cmp(&left.boundary_count))
        .then(left.offset.cmp(&right.offset))
        .then(left.symbol.name.cmp(&right.symbol.name))
        .then(left.uri.cmp(right.uri))
        .then(left.range_start.cmp(&right.range_start))
        .then(left_indices.cmp(&right.indices))
}

fn best_match_into(
    name: &str,
    query: &str,
    indices: &mut Vec<usize>,
    candidate: &mut Vec<usize>,
) -> Option<(usize, usize)> {
    if name.is_ascii() && query.is_ascii() {
        return best_match_ascii(name.as_bytes(), query.as_bytes(), indices, candidate);
    }
    best_match_unicode(name, query, indices, candidate)
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
        if !name[start].eq_ignore_ascii_case(&query[0]) {
            continue;
        }
        candidate.clear();
        candidate.push(start);
        let mut position = start + 1;
        for &wanted in &query[1..] {
            let Some(offset) = name[position..]
                .iter()
                .position(|value| value.eq_ignore_ascii_case(&wanted))
            else {
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

fn best_match_unicode(
    name: &str,
    query: &str,
    indices: &mut Vec<usize>,
    candidate: &mut Vec<usize>,
) -> Option<(usize, usize)> {
    if query.is_empty() {
        indices.clear();
        return Some((0, 0));
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
        candidate.clear();
        candidate.push(start);
        let mut position = start + 1;
        for row in next.iter().skip(1) {
            let index = row[position.min(name.len())];
            if index == name.len() {
                break;
            }
            candidate.push(index);
            position = index + 1;
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

fn is_subsequence_bytes(name: &[u8], query: &[u8]) -> bool {
    let mut query = query.iter();
    let Some(mut wanted) = query.next() else {
        return true;
    };
    for character in name {
        if character.eq_ignore_ascii_case(wanted) {
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

pub fn symbol_information(symbol: &Symbol, uri: &str, containers: &ContainerTable) -> Value {
    let [start_line, start_character, end_line, end_character] = symbol.range;
    crate::json!({"name": (symbol.name.clone()), "kind": (u64::from(symbol.kind)), "containerName": (containers.get(symbol.container).to_owned()), "location": {"uri": (uri.to_owned()), "range": {"start": {"line": (u64::from(start_line)), "character": (u64::from(start_character))}, "end": {"line": (u64::from(end_line)), "character": (u64::from(end_character))}}}})
}

pub fn write_symbol_information(
    symbol: &Symbol,
    uri: &str,
    containers: &ContainerTable,
    output: &mut Vec<u8>,
) {
    let [start_line, start_character, end_line, end_character] = symbol.range;
    output.extend_from_slice(b"{\"name\":");
    crate::json::write_string(&symbol.name, output);
    output.extend_from_slice(b",\"kind\":");
    write!(output, "{}", u64::from(symbol.kind)).expect("writing to Vec cannot fail");
    output.extend_from_slice(b",\"containerName\":");
    crate::json::write_string(containers.get(symbol.container), output);
    output.extend_from_slice(b",\"location\":{\"uri\":");
    crate::json::write_string(uri, output);
    output.extend_from_slice(b",\"range\":{\"start\":{\"line\":");
    write!(output, "{}", start_line).expect("writing to Vec cannot fail");
    output.extend_from_slice(b",\"character\":");
    write!(output, "{}", start_character).expect("writing to Vec cannot fail");
    output.extend_from_slice(b"},\"end\":{\"line\":");
    write!(output, "{}", end_line).expect("writing to Vec cannot fail");
    output.extend_from_slice(b",\"character\":");
    write!(output, "{}", end_character).expect("writing to Vec cannot fail");
    output.extend_from_slice(b"}}}}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol(name: &str, _uri: &str, line: u64) -> Symbol {
        let mut containers = ContainerTable::default();
        symbol_with_container(name, "", line, &mut containers)
    }

    fn symbol_with_container(
        name: &str,
        container: &str,
        line: u64,
        containers: &mut ContainerTable,
    ) -> Symbol {
        let container_name = if container.is_empty() {
            name.to_owned()
        } else {
            format!("{container}.{name}")
        };
        Symbol {
            name: name.to_owned(),
            folded_name: (!name.is_ascii()).then(|| name.to_lowercase()),
            folded_container_name: (!container_name.is_ascii())
                .then(|| container_name.to_lowercase()),
            boundary_positions: boundary_positions(name),
            container_boundary_positions: boundary_positions_parts(container, name),
            kind: 12,
            container: containers.intern(container),
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
        let mut indices = Vec::new();
        let mut candidate = Vec::new();
        best_match_unicode("rreaddy", "ready", &mut indices, &mut candidate).unwrap();
        assert_eq!(indices, vec![1, 2, 3, 4, 6]);
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
        let mut containers = ContainerTable::default();
        let items = [symbol_with_container("jump", "Player", 0, &mut containers)];
        let search =
            |query| search_with_uris(items.iter().map(|symbol| ("", symbol)), query, &containers);
        assert_eq!(search("player jump")[0].1.name, "jump");
        assert_eq!(search("Player.jump")[0].1.name, "jump");
        assert!(search("player").is_empty());
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
    fn real_name_search_benchmark() {
        const NAMES: [&str; 16] = [
            "_ready",
            "_process",
            "_physics_process",
            "spawn_enemy",
            "player_velocity",
            "take_damage",
            "on_area_entered",
            "PlayerController",
            "inventory_slot",
            "apply_impulse",
            "update_health_bar",
            "camera_shake",
            "save_game_state",
            "load_scene_async",
            "connect_signals",
            "queue_free_children",
        ];
        let items = (0..32_000)
            .map(|index| {
                symbol(
                    &format!("{}_{index}", NAMES[index as usize % NAMES.len()]),
                    "file:///a",
                    index,
                )
            })
            .collect::<Vec<_>>();
        for query in ["ready", "", "pv", "player jump"] {
            let start = std::time::Instant::now();
            let results = search(&items, query);
            println!("real-name search {query:?}: {:?}", start.elapsed());
            assert!(results.len() <= 200);
        }
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
        let mut containers = ContainerTable::default();
        let flattened = flatten(&result, "file:///tmp/../tmp/main.gd", &mut containers);
        assert_eq!(flattened.len(), 4);
        assert_eq!(containers.get(flattened[0].1.container), "");
        assert_eq!(containers.get(flattened[1].1.container), "Root");
        assert_eq!(flattened[0].1.range[0], 1);
        assert_eq!(flattened[0].0, "file:///tmp/main.gd");
        assert_eq!(flattened[1].0, "file:///tmp/main.gd");
        assert_eq!(flattened[2].0, "file:///tmp/other.gd");
        assert_eq!(flattened[3].0, "file:///tmp/other.gd");
    }
}

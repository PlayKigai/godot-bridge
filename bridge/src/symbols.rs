use crate::json::Value;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::io::Write;

const MAX_CONTAINER_ENTRIES: usize = 4096;

#[derive(Clone, Debug, PartialEq)]
pub struct Symbol {
    pub name: Box<str>,
    folded: Option<Box<Folded>>,
    boundary: u64,
    container_boundary: u64,
    occurrence_mask: u32,
    pub kind: u8,
    pub container: u32,
    pub range: [u32; 4],
}

#[derive(Clone, Debug, PartialEq)]
struct Folded {
    name: Option<Box<str>>,
    container_name: Option<Box<str>>,
    boundary: Option<Box<[u64]>>,
    container_boundary: Option<Box<[u64]>>,
    inline_container: Option<Box<str>>,
}

#[derive(Default)]
pub struct ContainerTable {
    names: Vec<String>,
    indices: HashMap<String, u32>,
}

impl ContainerTable {
    fn intern(&mut self, name: &str) -> Option<u32> {
        if let Some(&index) = self.indices.get(name) {
            return Some(index);
        }
        if self.names.len() >= MAX_CONTAINER_ENTRIES {
            return None;
        }
        let index = self.names.len() as u32;
        self.names.push(name.to_owned());
        self.indices.insert(name.to_owned(), index);
        Some(index)
    }

    pub(crate) fn clear(&mut self) {
        self.names.clear();
        self.indices.clear();
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
    let mut uri_cache = HashMap::new();
    let default_uri = normalize_uri_cached(default_uri, &mut uri_cache);
    let mut output = Vec::new();
    for item in items {
        if let Some(location) = item.get("location") {
            let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
            let container = item
                .get("containerName")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let uri = location
                .get("uri")
                .and_then(Value::as_str)
                .map(|uri| normalize_uri_cached(uri, &mut uri_cache))
                .unwrap_or_else(|| default_uri.clone());
            output.push((
                uri,
                make_symbol(
                    name,
                    container,
                    item.get("kind")
                        .and_then(Value::as_u64)
                        .and_then(|kind| u8::try_from(kind).ok())
                        .unwrap_or_default(),
                    range_values(location.get("range").unwrap_or(&Value::Null)),
                    containers,
                ),
            ));
        } else {
            flatten_document(
                item,
                &default_uri,
                "",
                &mut output,
                containers,
                &mut uri_cache,
            );
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
    uri_cache: &mut HashMap<String, String>,
) {
    let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
    let uri = item
        .get("uri")
        .and_then(Value::as_str)
        .map(|uri| normalize_uri_cached(uri, uri_cache))
        .unwrap_or_else(|| uri.to_owned());
    let container_name = if container.is_empty() {
        name.to_owned()
    } else {
        format!("{container}.{name}")
    };
    output.push((
        uri.clone(),
        make_symbol(
            name,
            container,
            item.get("kind")
                .and_then(Value::as_u64)
                .and_then(|kind| u8::try_from(kind).ok())
                .unwrap_or_default(),
            item.get("selectionRange")
                .or_else(|| item.get("range"))
                .map_or([0; 4], range_values),
            containers,
        ),
    ));
    if let Some(children) = item.get("children").and_then(Value::as_array) {
        for child in children {
            flatten_document(child, &uri, &container_name, output, containers, uri_cache);
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

fn normalize_uri_cached(uri: &str, cache: &mut HashMap<String, String>) -> String {
    if let Some(normalized) = cache.get(uri) {
        return normalized.clone();
    }
    let normalized = normalize_uri(uri);
    cache.insert(uri.to_owned(), normalized.clone());
    normalized
}

fn make_symbol(
    name: &str,
    container: &str,
    kind: u8,
    range: [u32; 4],
    containers: &mut ContainerTable,
) -> Symbol {
    let container_name = if container.is_empty() {
        name.to_owned()
    } else {
        format!("{container}.{name}")
    };
    let (container_index, inline_container) = containers
        .intern(container)
        .map_or((0, Some(container.into())), |index| (index, None));
    let (boundary, boundary_list) = boundary_positions(name);
    let (container_boundary, container_boundary_list) = boundary_positions(&container_name);
    let folded = make_folded(
        name,
        &container_name,
        boundary_list,
        container_boundary_list,
        inline_container,
    );
    Symbol {
        name: name.into(),
        folded,
        boundary,
        container_boundary,
        occurrence_mask: occurrence_mask(&container_name),
        kind,
        container: container_index,
        range,
    }
}

fn make_folded(
    name: &str,
    container_name: &str,
    boundary: Option<Box<[u64]>>,
    container_boundary: Option<Box<[u64]>>,
    inline_container: Option<Box<str>>,
) -> Option<Box<Folded>> {
    let folded_name = (!name.is_ascii()).then(|| name.to_lowercase().into_boxed_str());
    let folded_container_name =
        (!container_name.is_ascii()).then(|| container_name.to_lowercase().into_boxed_str());
    (folded_name.is_some()
        || folded_container_name.is_some()
        || boundary.is_some()
        || container_boundary.is_some()
        || inline_container.is_some())
    .then(|| {
        Box::new(Folded {
            name: folded_name,
            container_name: folded_container_name,
            boundary,
            container_boundary,
            inline_container,
        })
    })
}

fn boundary_positions(value: &str) -> (u64, Option<Box<[u64]>>) {
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
    let mask = positions
        .iter()
        .filter(|position| **position < 64)
        .fold(0, |mask, position| mask | (1 << position));
    let positions = positions
        .into_iter()
        .filter(|position| *position >= 64)
        .collect::<Vec<_>>();
    (
        mask,
        (!positions.is_empty()).then(|| positions.into_boxed_slice()),
    )
}

fn boundary_contains(mask: u64, positions: Option<&[u64]>, position: usize) -> bool {
    if position < 64 {
        mask & (1 << position) != 0
    } else {
        positions.is_some_and(|positions| {
            u64::try_from(position)
                .ok()
                .is_some_and(|position| positions.binary_search(&position).is_ok())
        })
    }
}

fn occurrence_mask(value: &str) -> u32 {
    value.bytes().fold(0, |mask, byte| {
        let byte = byte.to_ascii_lowercase();
        mask | (1
            << match byte {
                b'a'..=b'z' => byte - b'a',
                b'0'..=b'9' => 26,
                b'_' => 27,
                _ => 28,
            })
    })
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
    let query_mask = occurrence_mask(&query);
    let mut scratch = Vec::new();
    let mut candidate = Vec::new();
    let mut target = Vec::new();
    let mut matches = BinaryHeap::with_capacity(200);
    for (uri, symbol) in symbols {
        if symbol.occurrence_mask & query_mask != query_mask {
            continue;
        }
        let Some((gap, offset)) = (if container_query {
            if let Some(target) = symbol
                .folded
                .as_ref()
                .and_then(|folded| folded.container_name.as_deref())
            {
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
                    best_match_into(symbol.name.as_ref(), &query, &mut scratch, &mut candidate)
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
        } else if let Some(target) = symbol
            .folded
            .as_ref()
            .and_then(|folded| folded.name.as_deref())
        {
            if target.len() > 256 && target.chars().count() > 256 {
                continue;
            }
            best_match_into(target, &query, &mut scratch, &mut candidate)
        } else {
            if symbol.name.len() > 256 {
                continue;
            }
            best_match_into(symbol.name.as_ref(), &query, &mut scratch, &mut candidate)
        }) else {
            continue;
        };
        let (boundary, boundary_list) = if container_query {
            (
                symbol.container_boundary,
                symbol
                    .folded
                    .as_ref()
                    .and_then(|folded| folded.container_boundary.as_deref()),
            )
        } else {
            (
                symbol.boundary,
                symbol
                    .folded
                    .as_ref()
                    .and_then(|folded| folded.boundary.as_deref()),
            )
        };
        let boundary_count = scratch
            .iter()
            .filter(|index| boundary_contains(boundary, boundary_list, **index))
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
    let name = name.chars().collect::<Vec<_>>();
    let query = query.chars().collect::<Vec<_>>();
    if !is_subsequence(&name, &query, &|left, right| left == right) {
        return None;
    }
    best_match(&name, &query, indices, candidate, |left, right| {
        left == right
    })
}

fn best_match_ascii(
    name: &[u8],
    query: &[u8],
    indices: &mut Vec<usize>,
    candidate: &mut Vec<usize>,
) -> Option<(usize, usize)> {
    if !is_subsequence_bytes(name, query) {
        return None;
    }
    best_match(name, query, indices, candidate, |left, right| {
        left.eq_ignore_ascii_case(right)
    })
}

fn best_match<T, F>(
    name: &[T],
    query: &[T],
    indices: &mut Vec<usize>,
    candidate: &mut Vec<usize>,
    equal: F,
) -> Option<(usize, usize)>
where
    F: Fn(&T, &T) -> bool,
{
    if query.is_empty() {
        indices.clear();
        return Some((0, 0));
    }
    let mut best = None;
    for start in 0..name.len() {
        if !equal(&name[start], &query[0]) {
            continue;
        }
        candidate.clear();
        candidate.push(start);
        let mut position = start + 1;
        for wanted in &query[1..] {
            let Some(offset) = name[position..]
                .iter()
                .position(|value| equal(value, wanted))
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

fn is_subsequence<T, F>(name: &[T], query: &[T], equal: &F) -> bool
where
    F: Fn(&T, &T) -> bool,
{
    let mut query = query.iter();
    let Some(mut wanted) = query.next() else {
        return true;
    };
    for character in name {
        if equal(character, wanted) {
            let Some(next) = query.next() else {
                return true;
            };
            wanted = next;
        }
    }
    false
}

fn is_subsequence_bytes(name: &[u8], query: &[u8]) -> bool {
    is_subsequence(name, query, &u8::eq_ignore_ascii_case)
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

pub fn write_symbol_information(
    symbol: &Symbol,
    uri: &str,
    containers: &ContainerTable,
    output: &mut Vec<u8>,
) {
    let [start_line, start_character, end_line, end_character] = symbol.range;
    output.extend_from_slice(b"{\"name\":");
    crate::json::write_string(symbol.name.as_ref(), output);
    output.extend_from_slice(b",\"kind\":");
    write!(output, "{}", u64::from(symbol.kind)).expect("writing to Vec cannot fail");
    output.extend_from_slice(b",\"containerName\":");
    crate::json::write_string(
        symbol
            .folded
            .as_ref()
            .and_then(|folded| folded.inline_container.as_deref())
            .unwrap_or_else(|| containers.get(symbol.container)),
        output,
    );
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

    fn symbol(name: &str, line: u64) -> Symbol {
        let mut containers = ContainerTable::default();
        symbol_with_container(name, "", line, &mut containers)
    }

    fn symbol_with_container(
        name: &str,
        container: &str,
        line: u64,
        containers: &mut ContainerTable,
    ) -> Symbol {
        make_symbol(
            name,
            container,
            12,
            [line as u32, 0, line as u32, 1],
            containers,
        )
    }

    fn minimum_elapsed(mut operation: impl FnMut()) -> std::time::Duration {
        (0..10)
            .map(|_| {
                let start = std::time::Instant::now();
                operation();
                std::hint::black_box(());
                start.elapsed()
            })
            .min()
            .unwrap()
    }

    fn profile() -> &'static str {
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    }

    #[test]
    fn ranking_and_empty_query_are_stable() {
        let items = vec![
            symbol("a_ready", 0),
            symbol("_ready", 1),
            symbol("read_y", 0),
        ];
        assert_eq!(
            search(&items, "ready")
                .iter()
                .map(|item| item.name.as_ref())
                .collect::<Vec<_>>(),
            vec!["read_y", "_ready", "a_ready"]
        );
        assert_eq!(
            search(&items, "")
                .iter()
                .map(|item| item.name.as_ref())
                .collect::<Vec<_>>(),
            vec!["_ready", "a_ready", "read_y"]
        );
    }

    #[test]
    fn alignment_uses_gap_then_offset_then_indices() {
        let items = vec![
            symbol("rxxeady", 0),
            symbol("ready", 0),
            symbol("xready", 0),
        ];
        assert_eq!(
            search(&items, "ready").first().unwrap().name.as_ref(),
            "ready"
        );
        let mut indices = Vec::new();
        let mut candidate = Vec::new();
        best_match_into("rreaddy", "ready", &mut indices, &mut candidate).unwrap();
        assert_eq!(indices, vec![1, 2, 3, 4, 6]);
    }

    #[test]
    fn boundary_bonus_prefers_word_starts() {
        let items = vec![symbol("improve", 0), symbol("player_velocity", 0)];
        assert_eq!(search(&items, "pv")[0].name.as_ref(), "player_velocity");
    }

    #[test]
    fn container_queries_match_dotted_and_spaced_names() {
        let mut containers = ContainerTable::default();
        let items = [symbol_with_container("jump", "Player", 0, &mut containers)];
        let search =
            |query| search_with_uris(items.iter().map(|symbol| ("", symbol)), query, &containers);
        assert_eq!(search("player jump")[0].1.name.as_ref(), "jump");
        assert_eq!(search("Player.jump")[0].1.name.as_ref(), "jump");
        assert!(search("player").is_empty());
    }

    #[test]
    fn repeated_characters_match_quickly() {
        let items = vec![symbol(&"a".repeat(64), 0)];
        assert_eq!(search(&items, &"a".repeat(32)).len(), 1);
        assert!(search(&items, &format!("{}b", "a".repeat(32))).is_empty());
    }

    #[test]
    fn truncates_to_two_hundred() {
        let items = (0..201)
            .map(|index| symbol(&format!("x{index:03}"), index))
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
                    index,
                )
            })
            .collect::<Vec<_>>();
        for query in ["ready", "", "pv", "player jump"] {
            let query_mask = occurrence_mask(query);
            let expected = items
                .iter()
                .filter(|symbol| is_subsequence_bytes(symbol.name.as_bytes(), query.as_bytes()))
                .count();
            let masked = items
                .iter()
                .filter(|symbol| {
                    symbol.occurrence_mask & query_mask == query_mask
                        && is_subsequence_bytes(symbol.name.as_bytes(), query.as_bytes())
                })
                .count();
            assert_eq!(masked, expected);
            let results = search(&items, query);
            let elapsed = minimum_elapsed(|| {
                std::hint::black_box(search(&items, query));
            });
            println!(
                "symbols {} real-name search {query:?}: {elapsed:?}",
                profile()
            );
            assert!(results.len() <= 200);
        }
    }

    #[test]
    fn synthetic_search_benchmark_matches_reference() {
        let items = (0..32_000)
            .map(|index| symbol(&format!("symbol_{index:05}_ready"), index))
            .collect::<Vec<_>>();
        for query in ["ready", "", "zzz"] {
            let actual = search(&items, query);
            let elapsed = minimum_elapsed(|| {
                std::hint::black_box(search(&items, query));
            });
            match query {
                "ready" => assert_eq!(actual.len(), 200),
                "" => {
                    assert_eq!(actual.len(), 200);
                    assert_eq!(actual[0].name.as_ref(), "symbol_00000_ready");
                }
                "zzz" => assert!(actual.is_empty()),
                _ => unreachable!(),
            }
            println!("symbols {} search {query:?}: {elapsed:?}", profile());
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

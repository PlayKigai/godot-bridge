use crate::json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct Symbol {
    pub name: String,
    folded_name: String,
    folded_container_name: String,
    boundary_positions: Vec<usize>,
    container_boundary_positions: Vec<usize>,
    pub kind: Value,
    pub container: String,
    pub uri: String,
    pub range: Value,
}

#[derive(Debug)]
struct Match<'a> {
    symbol: &'a Symbol,
    gap: usize,
    offset: usize,
    indices: Vec<usize>,
    range_start: (u64, u64),
    boundary_count: usize,
}

pub fn flatten(result: &Value, default_uri: &str) -> Vec<Symbol> {
    let Some(items) = result.as_array() else {
        return Vec::new();
    };
    let default_uri = normalize_uri(default_uri);
    let mut output = Vec::new();
    for item in items {
        if item.get("location").is_some() {
            let location = item.get("location").unwrap_or(&Value::Null);
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
            output.push(Symbol {
                boundary_positions: boundary_positions(&name),
                container_boundary_positions: boundary_positions(&container_name),
                folded_container_name: container_name.to_lowercase(),
                name,
                folded_name,
                kind: item.get("kind").cloned().unwrap_or(Value::Null),
                container,
                uri,
                range: location.get("range").cloned().unwrap_or(Value::Null),
            });
        } else {
            flatten_document(item, &default_uri, "", &mut output);
        }
    }
    output
}

fn flatten_document(item: &Value, uri: &str, container: &str, output: &mut Vec<Symbol>) {
    let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
    let uri = item
        .get("uri")
        .and_then(Value::as_str)
        .map(normalize_uri)
        .unwrap_or_else(|| uri.to_owned());
    let folded_name = name.to_lowercase();
    let container_name = if container.is_empty() {
        name.to_owned()
    } else {
        format!("{container}.{name}")
    };
    let current_container = if container.is_empty() {
        String::new()
    } else {
        container.to_owned()
    };
    output.push(Symbol {
        name: name.to_owned(),
        folded_name,
        folded_container_name: container_name.to_lowercase(),
        boundary_positions: boundary_positions(name),
        container_boundary_positions: boundary_positions(&container_name),
        kind: item.get("kind").cloned().unwrap_or(Value::Null),
        container: current_container,
        uri: uri.clone(),
        range: item
            .get("selectionRange")
            .cloned()
            .or_else(|| item.get("range").cloned())
            .unwrap_or(Value::Null),
    });
    let next = if container.is_empty() {
        name.to_owned()
    } else {
        format!("{container}.{name}")
    };
    if let Some(children) = item.get("children").and_then(Value::as_array) {
        for child in children {
            flatten_document(child, &uri, &next, output);
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

fn boundary_positions(value: &str) -> Vec<usize> {
    let characters = value.chars().collect::<Vec<_>>();
    characters
        .iter()
        .enumerate()
        .filter_map(|(index, character)| {
            (index == 0
                || characters[index - 1] == '_'
                || (character.is_uppercase() && characters[index - 1].is_lowercase()))
            .then_some(index)
        })
        .collect()
}

pub fn search(symbols: &[Symbol], query: &str) -> Vec<Symbol> {
    let container_query = query.contains('.') || query.contains(' ');
    let query = query.to_lowercase();
    let query = if container_query {
        query.replace(' ', ".")
    } else {
        query
    };
    let mut matches = symbols
        .iter()
        .filter_map(|symbol| {
            let target = if container_query {
                &symbol.folded_container_name
            } else {
                &symbol.folded_name
            };
            let matched = best_match(target, &query)?;
            let boundary_positions = if container_query {
                &symbol.container_boundary_positions
            } else {
                &symbol.boundary_positions
            };
            let boundary_count = matched
                .2
                .iter()
                .filter(|index| boundary_positions.binary_search(index).is_ok())
                .count();
            Some(Match {
                symbol,
                gap: matched.0.saturating_sub(boundary_count * 4),
                offset: matched.1,
                boundary_count,
                indices: matched.2,
                range_start: range_start(&symbol.range),
            })
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| {
        left.gap
            .cmp(&right.gap)
            .then(right.boundary_count.cmp(&left.boundary_count))
            .then(left.offset.cmp(&right.offset))
            .then(left.symbol.name.cmp(&right.symbol.name))
            .then(left.symbol.uri.cmp(&right.symbol.uri))
            .then(left.range_start.cmp(&right.range_start))
            .then(left.indices.cmp(&right.indices))
    });
    matches
        .into_iter()
        .take(200)
        .map(|matched| matched.symbol.clone())
        .collect()
}

fn best_match(name: &str, query: &str) -> Option<(usize, usize, Vec<usize>)> {
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

fn range_start(range: &Value) -> (u64, u64) {
    let start = range.get("start").unwrap_or(&Value::Null);
    (
        start.get("line").and_then(Value::as_u64).unwrap_or(0),
        start.get("character").and_then(Value::as_u64).unwrap_or(0),
    )
}

pub fn symbol_information(symbol: &Symbol) -> Value {
    crate::json!({"name": (symbol.name.clone()), "kind": (symbol.kind.clone()), "containerName": (symbol.container.clone()), "location": {"uri": (symbol.uri.clone()), "range": (symbol.range.clone())}})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol(name: &str, uri: &str, line: u64) -> Symbol {
        symbol_with_container(name, "", uri, line)
    }

    fn symbol_with_container(name: &str, container: &str, uri: &str, line: u64) -> Symbol {
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
            kind: crate::json!(12),
            container: container.to_owned(),
            uri: uri.to_owned(),
            range: crate::json!({"start":{"line":line,"character":0},"end":{"line":line,"character":1}}),
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
            best_match("rreaddy", "ready").unwrap().2,
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
        let items = vec![symbol_with_container("jump", "Player", "file:///a", 0)];
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
        assert_eq!(flattened[0].container, "");
        assert_eq!(flattened[1].container, "Root");
        assert_eq!(flattened[0].range["start"]["line"], 1);
        assert_eq!(flattened[0].uri, "file:///tmp/main.gd");
        assert_eq!(flattened[1].uri, "file:///tmp/main.gd");
        assert_eq!(flattened[2].uri, "file:///tmp/other.gd");
        assert_eq!(flattened[3].uri, "file:///tmp/other.gd");
    }
}

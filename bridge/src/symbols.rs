use serde_json::{json, Value};

#[derive(Clone, Debug, PartialEq)]
pub struct Symbol {
    pub name: String,
    folded_name: String,
    pub kind: Value,
    pub container: String,
    pub uri: String,
    pub range: Value,
}

#[derive(Clone, Debug)]
struct Match {
    symbol: Symbol,
    gap: usize,
    offset: usize,
    indices: Vec<usize>,
}

pub fn flatten(result: &Value, default_uri: &str) -> Vec<Symbol> {
    let Some(items) = result.as_array() else {
        return Vec::new();
    };
    let mut output = Vec::new();
    for item in items {
        if item.get("location").is_some() {
            let location = item.get("location").unwrap_or(&Value::Null);
            let uri = normalize_uri(
                location
                    .get("uri")
                    .and_then(Value::as_str)
                    .unwrap_or(default_uri),
            );
            output.push(Symbol {
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                folded_name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_lowercase(),
                kind: item.get("kind").cloned().unwrap_or(Value::Null),
                container: item
                    .get("containerName")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                uri,
                range: location.get("range").cloned().unwrap_or(Value::Null),
            });
        } else {
            flatten_document(item, default_uri, "", &mut output);
        }
    }
    output
}

fn flatten_document(item: &Value, uri: &str, container: &str, output: &mut Vec<Symbol>) {
    let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
    let current_container = if container.is_empty() {
        String::new()
    } else {
        container.to_owned()
    };
    output.push(Symbol {
        name: name.to_owned(),
        folded_name: name.to_lowercase(),
        kind: item.get("kind").cloned().unwrap_or(Value::Null),
        container: current_container,
        uri: normalize_uri(item.get("uri").and_then(Value::as_str).unwrap_or(uri)),
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
            flatten_document(child, uri, &next, output);
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

pub fn search(symbols: &[Symbol], query: &str) -> Vec<Symbol> {
    let query = query.to_lowercase();
    let mut matches = symbols
        .iter()
        .filter_map(|symbol| {
            let matched = best_match(&symbol.folded_name, &query)?;
            Some(Match {
                symbol: symbol.clone(),
                gap: matched.0,
                offset: matched.1,
                indices: matched.2,
            })
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| {
        left.gap
            .cmp(&right.gap)
            .then(left.offset.cmp(&right.offset))
            .then(left.symbol.name.cmp(&right.symbol.name))
            .then(left.symbol.uri.cmp(&right.symbol.uri))
            .then(range_start(&left.symbol.range).cmp(&range_start(&right.symbol.range)))
            .then(left.indices.cmp(&right.indices))
    });
    matches
        .into_iter()
        .take(200)
        .map(|matched| matched.symbol)
        .collect()
}

fn best_match(name: &str, query: &str) -> Option<(usize, usize, Vec<usize>)> {
    if query.is_empty() {
        return Some((0, 0, Vec::new()));
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

fn range_start(range: &Value) -> (u64, u64) {
    let start = range.get("start").unwrap_or(&Value::Null);
    (
        start.get("line").and_then(Value::as_u64).unwrap_or(0),
        start.get("character").and_then(Value::as_u64).unwrap_or(0),
    )
}

pub fn symbol_information(symbol: &Symbol) -> Value {
    json!({"name": symbol.name, "kind": symbol.kind, "containerName": symbol.container, "location": {"uri": symbol.uri, "range": symbol.range}})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol(name: &str, uri: &str, line: u64) -> Symbol {
        Symbol {
            name: name.to_owned(),
            folded_name: name.to_lowercase(),
            kind: json!(12),
            container: String::new(),
            uri: uri.to_owned(),
            range: json!({"start":{"line":line,"character":0},"end":{"line":line,"character":1}}),
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
            vec!["_ready", "a_ready", "read_y"]
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
    fn flattens_nested_document_symbols() {
        let result = json!([{"name":"Root","kind":5,"range":{"start":{"line":0},"end":{"line":4}},"selectionRange":{"start":{"line":1},"end":{"line":1}},"children":[{"name":"Child","kind":6,"range":{"start":{"line":2},"end":{"line":3}},"selectionRange":{"start":{"line":2},"end":{"line":2}}}]}]);
        let flattened = flatten(&result, "file:///tmp/main.gd");
        assert_eq!(flattened.len(), 2);
        assert_eq!(flattened[0].container, "");
        assert_eq!(flattened[1].container, "Root");
        assert_eq!(flattened[0].range["start"]["line"], 1);
    }
}

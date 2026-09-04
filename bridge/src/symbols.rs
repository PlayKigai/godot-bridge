//! Document symbol caching and workspace symbol search.

use serde_json::{json, Value};

#[derive(Clone, Debug, PartialEq)]
pub struct Symbol {
    pub name: String,
    pub kind: Value,
    pub container: String,
    pub uri: String,
    pub range: Value,
}

#[derive(Clone, Debug)]
pub struct Match {
    pub symbol: Symbol,
    pub gap: usize,
    pub offset: usize,
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
            let matched = best_match(&symbol.name, &query)?;
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
    let name = name.to_lowercase().chars().collect::<Vec<_>>();
    let query = query.chars().collect::<Vec<_>>();
    let mut best: Option<(usize, usize, Vec<usize>)> = None;
    fn visit(
        name: &[char],
        query: &[char],
        qi: usize,
        start: usize,
        indices: &mut Vec<usize>,
        best: &mut Option<(usize, usize, Vec<usize>)>,
    ) {
        if qi == query.len() {
            let gap = indices.windows(2).map(|pair| pair[1] - pair[0] - 1).sum();
            let candidate = (gap, start, indices.clone());
            if best.as_ref().is_none_or(|current| candidate < *current) {
                *best = Some(candidate);
            }
            return;
        }
        for index in indices.last().map_or(0, |index| index + 1)..name.len() {
            if name[index] == query[qi] {
                indices.push(index);
                visit(
                    name,
                    query,
                    qi + 1,
                    if qi == 0 { index } else { start },
                    indices,
                    best,
                );
                indices.pop();
            }
        }
    }
    visit(&name, &query, 0, 0, &mut Vec::new(), &mut best);
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

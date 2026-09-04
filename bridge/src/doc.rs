//! Godot class reference lookup.

use std::process::Command;

const DOC_BASE: &str = "https://docs.godotengine.org/en/stable/classes/";

pub fn open_doc(symbol: &str) -> anyhow::Result<()> {
    let status = Command::new("xdg-open").arg(doc_url(symbol)).status()?;
    if !status.success() {
        anyhow::bail!("xdg-open exited {status}");
    }
    Ok(())
}

pub fn doc_url(symbol: &str) -> String {
    let (class, member) = symbol.split_once('.').unwrap_or((symbol, ""));
    let class = class.to_ascii_lowercase();
    if member.is_empty() {
        format!("{DOC_BASE}class_{class}.html")
    } else {
        format!(
            "{DOC_BASE}class_{class}.html#class-{class}-method-{}",
            member.to_ascii_lowercase()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_class_url() {
        assert_eq!(
            doc_url("Node2D"),
            "https://docs.godotengine.org/en/stable/classes/class_node2d.html"
        );
    }

    #[test]
    fn builds_member_anchor() {
        assert_eq!(
            doc_url("Node2D.get_position"),
            "https://docs.godotengine.org/en/stable/classes/class_node2d.html#class-node2d-method-get_position"
        );
    }
}

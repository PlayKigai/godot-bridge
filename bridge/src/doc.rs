use std::process::Command;

const DOC_BASE: &str = "https://docs.godotengine.org/en/stable/classes/";

pub fn open_doc(symbol: &str) -> crate::error::Result<()> {
    let root = crate::root::cwd_root()?;
    crate::settings_file::load_zed_settings(&root)?;
    let status = Command::new("xdg-open").arg(doc_url(symbol)?).status()?;
    if !status.success() {
        crate::bail!("xdg-open exited {status}");
    }
    Ok(())
}

pub fn doc_url(symbol: &str) -> crate::error::Result<String> {
    if symbol.len() > 256 {
        crate::bail!("invalid documentation symbol");
    }
    let mut parts = symbol.split('.');
    let class = parts.next().unwrap_or_default();
    let member = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || !valid_identifier(class)
        || (!member.is_empty() && !valid_identifier(member))
    {
        crate::bail!("invalid documentation symbol");
    }
    let class = class.to_ascii_lowercase();
    if member.is_empty() {
        Ok(format!("{DOC_BASE}class_{class}.html"))
    } else {
        Ok(format!(
            "{DOC_BASE}class_{class}.html#class-{class}-method-{}",
            member.to_ascii_lowercase()
        ))
    }
}

fn valid_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_class_url() {
        assert_eq!(
            doc_url("Node2D").unwrap(),
            "https://docs.godotengine.org/en/stable/classes/class_node2d.html".to_owned()
        );
    }

    #[test]
    fn builds_member_anchor() {
        assert_eq!(
            doc_url("Node2D.get_position").unwrap(),
            "https://docs.godotengine.org/en/stable/classes/class_node2d.html#class-node2d-method-get_position"
                .to_owned()
        );
    }
}

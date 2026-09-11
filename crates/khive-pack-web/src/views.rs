use std::path::{Component, Path};

use serde_json::{Map, Value};

pub(crate) fn read_view(source: &Path, declared_path: &str) -> Result<Option<Value>, String> {
    let relative = declared_path.trim_start_matches('/');
    if relative.is_empty()
        || declared_path.contains(['\\', '\0', '?', '#'])
        || declared_path.contains("://")
        || Path::new(relative)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("markdown_url must be a local path within the source tree".to_string());
    }
    let path = source.join(relative);
    let resolved = match path.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot resolve machine view: {}", error.kind())),
    };
    if !resolved.starts_with(source) {
        return Err("markdown_url resolves outside the source tree".to_string());
    }
    let text = std::fs::read_to_string(resolved)
        .map_err(|error| format!("cannot read machine view: {}", error.kind()))?;
    parse_frontmatter(&text).map(Some)
}

fn parse_frontmatter(text: &str) -> Result<Value, String> {
    let mut lines = text.trim_start_matches('\u{feff}').lines();
    if lines.next() != Some("---") {
        return Ok(Value::Object(Map::new()));
    }
    let mut yaml = String::new();
    let mut closed = false;
    for line in lines {
        if line == "---" || line == "..." {
            closed = true;
            break;
        }
        yaml.push_str(line);
        yaml.push('\n');
    }
    if !closed {
        return Err("machine-view frontmatter has no closing delimiter".to_string());
    }
    if yaml.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let value: serde_yaml::Value = serde_yaml::from_str(&yaml)
        .map_err(|_| "machine-view frontmatter is malformed YAML".to_string())?;
    let value = serde_json::to_value(value)
        .map_err(|_| "machine-view frontmatter must have string keys".to_string())?;
    if !value.is_object() {
        return Err("machine-view frontmatter must be an object".to_string());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preserves_declared_frontmatter_values() {
        assert_eq!(
            parse_frontmatter("---\npage_type: article\naeo:\n  domain: botany\n  quality_score: 0.42\n---\nBody\n").unwrap(),
            json!({"page_type":"article", "aeo":{"domain":"botany", "quality_score":0.42}})
        );
        assert_eq!(parse_frontmatter("# Plain page\n").unwrap(), json!({}));
        assert!(parse_frontmatter("---\npage_type: article\n").is_err());
    }

    #[test]
    fn distinguishes_missing_files_from_invalid_paths() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().canonicalize().unwrap();
        assert_eq!(read_view(&source, "/missing.md").unwrap(), None);
        for path in [
            "/../outside.md",
            "https://example.invalid/view.md",
            "/a\\b.md",
        ] {
            assert!(read_view(&source, path).is_err(), "{path}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinks_outside_the_source_tree() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("site");
        std::fs::create_dir(&source).unwrap();
        let outside = root.path().join("outside.md");
        std::fs::write(&outside, "---\npage_type: private\n---\n").unwrap();
        std::os::unix::fs::symlink(&outside, source.join("view.md")).unwrap();
        assert!(read_view(&source.canonicalize().unwrap(), "/view.md").is_err());
    }
}

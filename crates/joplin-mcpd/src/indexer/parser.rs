use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::{BTreeSet, HashMap};
use thiserror::Error;

static ID_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^id:\s+[0-9a-f]{32}$").expect("valid id regex"));
static FOOTER_LINE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[A-Za-z0-9_]+:\s*.*$").expect("valid footer regex"));
static RESOURCE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"\(:/([0-9a-f]{32})\)|src=["']:/([0-9a-f]{32})["']"#)
        .expect("valid resource regex")
});
static REQUIRED_KEYS: &[&str] = &["id", "type_", "created_time", "updated_time"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedItem {
    pub title: String,
    pub body: String,
    pub metadata: HashMap<String, String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseItemError {
    #[error("missing metadata footer")]
    MissingMetadataFooter,
    #[error("missing title")]
    MissingTitle,
    #[error("invalid metadata line")]
    InvalidMetadataLine,
    #[error("duplicated metadata key {key}")]
    DuplicatedMetadataKey { key: String },
    #[error("missing required metadata key {key}")]
    MissingRequiredMetadataKey { key: &'static str },
}

pub fn parse_joplin_item(raw: &str) -> Result<ParsedItem, ParseItemError> {
    let lines: Vec<&str> = raw.lines().collect();
    let mut footer_error = None;
    let mut metadata_start = None;

    for (idx, line) in lines.iter().enumerate().rev() {
        if !ID_RE.is_match(line.trim()) {
            continue;
        }

        match validate_metadata_footer(&lines[idx..]) {
            Ok(()) if !has_adjacent_required_key_duplicate(&lines, idx) => {
                metadata_start = Some(idx);
                break;
            }
            Ok(()) => {
                footer_error = Some(ParseItemError::DuplicatedMetadataKey {
                    key: "required footer key".to_string(),
                });
            }
            Err(err) => footer_error = Some(err),
        }
    }

    let metadata_start = metadata_start
        .ok_or_else(|| footer_error.unwrap_or(ParseItemError::MissingMetadataFooter))?;

    let title_pos = lines[..metadata_start]
        .iter()
        .position(|line| !line.trim().is_empty())
        .ok_or(ParseItemError::MissingTitle)?;

    let title = lines[title_pos].trim().to_string();
    let body = lines[(title_pos + 1)..metadata_start]
        .join("\n")
        .trim()
        .to_string();

    let mut metadata = HashMap::new();
    for line in &lines[metadata_start..] {
        if line.trim().is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once(':')
            .ok_or(ParseItemError::InvalidMetadataLine)?;
        if metadata
            .insert(key.trim().to_string(), value.trim().to_string())
            .is_some()
        {
            return Err(ParseItemError::DuplicatedMetadataKey {
                key: key.trim().to_string(),
            });
        }
    }

    Ok(ParsedItem {
        title,
        body,
        metadata,
    })
}

fn validate_metadata_footer(lines: &[&str]) -> Result<(), ParseItemError> {
    if lines.is_empty() {
        return Err(ParseItemError::MissingMetadataFooter);
    }

    let mut keys = BTreeSet::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !FOOTER_LINE_RE.is_match(trimmed) {
            return Err(ParseItemError::InvalidMetadataLine);
        }
        let Some((key, _)) = trimmed.split_once(':') else {
            return Err(ParseItemError::InvalidMetadataLine);
        };
        if !keys.insert(key.trim()) {
            return Err(ParseItemError::DuplicatedMetadataKey {
                key: key.trim().to_string(),
            });
        }
    }

    for key in REQUIRED_KEYS {
        if !keys.contains(key) {
            return Err(ParseItemError::MissingRequiredMetadataKey { key });
        }
    }

    Ok(())
}

fn has_adjacent_required_key_duplicate(lines: &[&str], candidate_idx: usize) -> bool {
    let footer_keys: BTreeSet<&str> = lines[candidate_idx..]
        .iter()
        .filter_map(|line| line.trim().split_once(':').map(|(key, _)| key.trim()))
        .collect();

    for line in lines[..candidate_idx].iter().rev() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return false;
        }
        let Some((key, _)) = trimmed.split_once(':') else {
            return false;
        };
        let key = key.trim();
        if REQUIRED_KEYS.contains(&key) && footer_keys.contains(key) {
            return true;
        }
    }
    false
}

pub fn extract_resource_refs(body: &str) -> Vec<String> {
    let mut refs = BTreeSet::new();
    for captures in RESOURCE_RE.captures_iter(body) {
        if let Some(id) = captures.get(1).or_else(|| captures.get(2)) {
            refs.insert(id.as_str().to_string());
        }
    }
    refs.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(body: &str) -> String {
        format!(
            "Example title\n\n{body}\n\nid: 0123456789abcdef0123456789abcdef\ntype_: 1\ncreated_time: 1\nupdated_time: 2\n"
        )
    }

    #[test]
    fn parses_note_item() {
        let parsed = parse_joplin_item(&note("Body text")).expect("valid item");
        assert_eq!(parsed.title, "Example title");
        assert_eq!(parsed.body, "Body text");
        assert_eq!(
            parsed.metadata.get("id"),
            Some(&"0123456789abcdef0123456789abcdef".to_string())
        );
    }

    #[test]
    fn parses_empty_body() {
        let parsed = parse_joplin_item(&note("")).expect("valid item");
        assert_eq!(parsed.body, "");
    }

    #[test]
    fn parses_notebook_item() {
        let raw = "Notebook title\n\nid: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\nparent_id: \ncreated_time: 1\nupdated_time: 2\ntype_: 2\n";
        let parsed = parse_joplin_item(raw).expect("valid notebook item");
        assert_eq!(parsed.title, "Notebook title");
        assert_eq!(parsed.body, "");
        assert_eq!(parsed.metadata.get("parent_id"), Some(&String::new()));
        assert_eq!(parsed.metadata.get("type_"), Some(&"2".to_string()));
    }

    #[test]
    fn rejects_malformed_item_without_footer() {
        let error =
            parse_joplin_item("Title\n\nBody without metadata").expect_err("footer is required");
        assert_eq!(error, ParseItemError::MissingMetadataFooter);
    }

    #[test]
    fn rejects_footer_missing_required_metadata() {
        let raw = "Title\n\nid: 0123456789abcdef0123456789abcdef\ntype_: 1\n";
        let error = parse_joplin_item(raw).expect_err("missing footer keys");
        assert_eq!(
            error,
            ParseItemError::MissingRequiredMetadataKey {
                key: "created_time"
            }
        );
    }

    #[test]
    fn rejects_duplicated_required_footer_keys() {
        let raw = "Title\n\nid: 0123456789abcdef0123456789abcdef\nid: 0123456789abcdef0123456789abcdef\ntype_: 1\ncreated_time: 1\nupdated_time: 2\n";
        let error = parse_joplin_item(raw).expect_err("duplicate keys");
        assert_eq!(
            error,
            ParseItemError::DuplicatedMetadataKey {
                key: "id".to_string()
            }
        );
    }

    #[test]
    fn does_not_split_body_on_pasted_valid_id_line() {
        let parsed = parse_joplin_item(&note(
            "```text\nid: 11111111111111111111111111111111\n```\nAfter block",
        ))
        .expect("valid item");
        assert!(parsed.body.contains("After block"));
    }

    #[test]
    fn handles_metadata_values_with_colons_and_empty_values() {
        let raw = "Title\n\nBody\n\nid: 0123456789abcdef0123456789abcdef\nsource_url: https://example.test/path:with:colons\nparent_id: \ntype_: 1\ncreated_time: 1\nupdated_time: 2\n";
        let parsed = parse_joplin_item(raw).expect("valid item");
        assert_eq!(
            parsed.metadata.get("source_url"),
            Some(&"https://example.test/path:with:colons".to_string())
        );
        assert_eq!(parsed.metadata.get("parent_id"), Some(&String::new()));
    }

    #[test]
    fn extracts_and_deduplicates_resource_refs() {
        let refs = extract_resource_refs(
            r#"![one](:/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)
               <img src=":/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb">
               <img src=':/cccccccccccccccccccccccccccccccc'>
               ![dupe](:/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)"#,
        );
        assert_eq!(
            refs,
            [
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
                "cccccccccccccccccccccccccccccccc".to_string()
            ]
        );
    }

    #[test]
    fn ignores_bare_resource_like_text() {
        let refs = extract_resource_refs("bare :/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa text");
        assert!(refs.is_empty());
    }
}

use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::{BTreeSet, HashMap};

static ID_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^id:\s+[0-9a-f]{32}$").expect("valid id regex"));
static FOOTER_LINE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[A-Za-z0-9_]+:\s*.*$").expect("valid footer regex"));
static RESOURCE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#":/([0-9a-f]{32})|src=["']:/([0-9a-f]{32})["']"#).expect("valid resource regex")
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedItem {
    pub title: String,
    pub body: String,
    pub metadata: HashMap<String, String>,
}

pub fn parse_joplin_item(raw: &str) -> anyhow::Result<ParsedItem> {
    let lines: Vec<&str> = raw.lines().collect();
    let metadata_start = lines
        .iter()
        .enumerate()
        .rev()
        .find_map(|(idx, line)| {
            if ID_RE.is_match(line.trim())
                && is_valid_metadata_footer(&lines[idx..])
                && !has_adjacent_required_key_duplicate(&lines, idx)
            {
                Some(idx)
            } else {
                None
            }
        })
        .ok_or_else(|| anyhow::anyhow!("missing metadata footer"))?;

    let title_pos = lines[..metadata_start]
        .iter()
        .position(|line| !line.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing title"))?;

    let title = lines[title_pos].trim().to_string();
    let body = lines[(title_pos + 1)..metadata_start]
        .join("\n")
        .trim()
        .to_string();

    let mut metadata = HashMap::new();
    for line in &lines[metadata_start..] {
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("invalid metadata line"))?;
        if metadata
            .insert(key.trim().to_string(), value.trim().to_string())
            .is_some()
        {
            anyhow::bail!("duplicated metadata key {key}");
        }
    }

    Ok(ParsedItem {
        title,
        body,
        metadata,
    })
}

fn is_valid_metadata_footer(lines: &[&str]) -> bool {
    if lines.is_empty() {
        return false;
    }

    let mut keys = BTreeSet::new();
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !FOOTER_LINE_RE.is_match(trimmed) {
            return false;
        }
        let Some((key, _)) = trimmed.split_once(':') else {
            return false;
        };
        if !keys.insert(key.trim()) {
            return false;
        }
    }

    ["id", "type_", "created_time", "updated_time"]
        .iter()
        .all(|key| keys.contains(key))
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
        if ["id", "type_", "created_time", "updated_time"].contains(&key)
            && footer_keys.contains(key)
        {
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
    fn rejects_footer_missing_required_metadata() {
        let raw = "Title\n\nid: 0123456789abcdef0123456789abcdef\ntype_: 1\n";
        let error = parse_joplin_item(raw).expect_err("missing footer keys");
        assert!(error.to_string().contains("missing metadata footer"));
    }

    #[test]
    fn rejects_duplicated_required_footer_keys() {
        let raw = "Title\n\nid: 0123456789abcdef0123456789abcdef\nid: 0123456789abcdef0123456789abcdef\ntype_: 1\ncreated_time: 1\nupdated_time: 2\n";
        let error = parse_joplin_item(raw).expect_err("duplicate keys");
        assert!(error.to_string().contains("missing metadata footer"));
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
    fn extracts_and_deduplicates_resource_refs() {
        let refs = extract_resource_refs(
            r#"![one](:/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)
               <img src=":/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb">
               ![dupe](:/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)"#,
        );
        assert_eq!(
            refs,
            [
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string()
            ]
        );
    }
}

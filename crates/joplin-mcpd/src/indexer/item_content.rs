use crate::indexer::JoplinItemType;
use crate::indexer::parser::{ParsedItem, parse_joplin_item};
use crate::indexer::source::JoplinItem;
use std::collections::HashMap;

pub fn parse_index_item(item: &JoplinItem, raw: &str) -> Option<ParsedItem> {
    match parse_joplin_item(raw) {
        Ok(parsed) => Some(parsed),
        Err(_) if item.item_type != JoplinItemType::Revision => {
            Some(parsed_from_db_columns(item, raw))
        }
        Err(_) => None,
    }
}

fn parsed_from_db_columns(item: &JoplinItem, raw: &str) -> ParsedItem {
    let mut metadata = loose_metadata(raw);
    metadata
        .entry("id".to_string())
        .or_insert_with(|| item.jop_id.clone());
    metadata
        .entry("type_".to_string())
        .or_insert_with(|| (item.item_type as i32).to_string());
    metadata
        .entry("created_time".to_string())
        .or_insert_with(|| item.created_time.to_string());
    metadata
        .entry("updated_time".to_string())
        .or_insert_with(|| item.updated_time.to_string());
    if !item.jop_parent_id.trim().is_empty() {
        metadata
            .entry("parent_id".to_string())
            .or_insert_with(|| item.jop_parent_id.clone());
    }

    match item.item_type {
        JoplinItemType::Note => ParsedItem {
            title: item.name.clone(),
            body: raw.to_string(),
            metadata,
        },
        JoplinItemType::Folder | JoplinItemType::Tag | JoplinItemType::Resource => ParsedItem {
            title: item.name.clone(),
            body: String::new(),
            metadata,
        },
        JoplinItemType::NoteTag => ParsedItem {
            title: String::new(),
            body: String::new(),
            metadata,
        },
        JoplinItemType::Revision => ParsedItem {
            title: String::new(),
            body: String::new(),
            metadata,
        },
    }
}

fn loose_metadata(raw: &str) -> HashMap<String, String> {
    if let Ok(Some(metadata)) = json_metadata(raw) {
        return metadata;
    }

    raw.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            let key = key.trim();
            if key.is_empty()
                || !key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            {
                return None;
            }
            Some((key.to_string(), value.trim().to_string()))
        })
        .collect()
}

fn json_metadata(raw: &str) -> Result<Option<HashMap<String, String>>, serde_json::Error> {
    if !raw.trim_start().starts_with('{') {
        return Ok(None);
    }

    let value: serde_json::Value = serde_json::from_str(raw)?;
    let Some(object) = value.as_object() else {
        return Ok(None);
    };
    let metadata = object
        .iter()
        .map(|(key, value)| {
            let value = match value {
                serde_json::Value::String(value) => value.clone(),
                serde_json::Value::Null => String::new(),
                _ => value.to_string(),
            };
            (key.clone(), value)
        })
        .collect();
    Ok(Some(metadata))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(jop_id: &str, item_type: JoplinItemType, content: &str) -> JoplinItem {
        JoplinItem {
            id: format!("server-{jop_id}"),
            owner_id: "owner".to_string(),
            content: content.as_bytes().to_vec(),
            name: format!("{jop_id} title"),
            mime_type: "text/plain".to_string(),
            updated_time: 20,
            created_time: 10,
            jop_id: jop_id.to_string(),
            jop_parent_id: "parent1".to_string(),
            item_type,
            encrypted: false,
        }
    }

    #[test]
    fn preserves_sync_file_parser_when_footer_exists() {
        let parsed = parse_index_item(
            &item("note1", JoplinItemType::Note, ""),
            "Sync title\n\nSync body\n\nid: 0123456789abcdef0123456789abcdef\ntype_: 1\ncreated_time: 1\nupdated_time: 2\n",
        )
        .expect("parsed sync item");

        assert_eq!(parsed.title, "Sync title");
        assert_eq!(parsed.body, "Sync body");
        assert_eq!(parsed.metadata.get("updated_time"), Some(&"2".to_string()));
    }

    #[test]
    fn falls_back_to_database_columns_for_live_note_content() {
        let parsed = parse_index_item(
            &item(
                "note1",
                JoplinItemType::Note,
                "Live body without metadata footer",
            ),
            "Live body without metadata footer",
        )
        .expect("parsed live item");

        assert_eq!(parsed.title, "note1 title");
        assert_eq!(parsed.body, "Live body without metadata footer");
        assert_eq!(parsed.metadata.get("id"), Some(&"note1".to_string()));
        assert_eq!(
            parsed.metadata.get("parent_id"),
            Some(&"parent1".to_string())
        );
        assert_eq!(parsed.metadata.get("updated_time"), Some(&"20".to_string()));
    }

    #[test]
    fn falls_back_to_loose_metadata_for_live_note_tag_content() {
        let parsed = parse_index_item(
            &item(
                "edge1",
                JoplinItemType::NoteTag,
                "note_id: note1\ntag_id: tag1",
            ),
            "note_id: note1\ntag_id: tag1",
        )
        .expect("parsed live note tag");

        assert_eq!(parsed.metadata.get("note_id"), Some(&"note1".to_string()));
        assert_eq!(parsed.metadata.get("tag_id"), Some(&"tag1".to_string()));
    }

    #[test]
    fn falls_back_to_json_metadata_for_live_note_tag_content() {
        let parsed = parse_index_item(
            &item(
                "edge1",
                JoplinItemType::NoteTag,
                r#"{"tag_id":"tag1","note_id":"note1","created_time":123}"#,
            ),
            r#"{"tag_id":"tag1","note_id":"note1","created_time":123}"#,
        )
        .expect("parsed live JSON note tag");

        assert_eq!(parsed.metadata.get("note_id"), Some(&"note1".to_string()));
        assert_eq!(parsed.metadata.get("tag_id"), Some(&"tag1".to_string()));
        assert_eq!(
            parsed.metadata.get("created_time"),
            Some(&"123".to_string())
        );
    }

    #[test]
    fn skips_revisions_without_sync_footer() {
        assert!(
            parse_index_item(&item("rev1", JoplinItemType::Revision, "body"), "body").is_none()
        );
    }
}

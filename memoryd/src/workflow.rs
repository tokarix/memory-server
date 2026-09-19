//! Strict classification of task-scoped workflow provenance.

use uuid::Uuid;

const TASK_PREFIX: &str = "task:";
const LEGACY_TASK_PREFIX: &str = "superseded-for-task:";
const SUPERSEDES_PREFIX: &str = "supersedes-plan:";
const REVIEWED_ITEM_PREFIX: &str = "reviewed-item:";

/// Return whether plan tags contain exact task-scoping evidence.
#[must_use]
pub fn plan_is_directly_scoped(tags: &[String]) -> bool {
    tags.iter().any(|tag| {
        parse_exact_uuid(tag, TASK_PREFIX).is_some()
            || parse_exact_uuid(tag, LEGACY_TASK_PREFIX).is_some()
    })
}

/// Extract exact superseded-plan targets.
#[must_use]
pub fn superseded_plan_ids(tags: &[String]) -> Vec<Uuid> {
    exact_uuid_values(tags, SUPERSEDES_PREFIX)
}

/// Extract exact reviewed-item targets.
#[must_use]
pub fn reviewed_item_ids(tags: &[String]) -> Vec<Uuid> {
    exact_uuid_values(tags, REVIEWED_ITEM_PREFIX)
}

/// Return whether free-form text contains a delimited, valid task token.
#[must_use]
pub fn contains_task_token(text: &str) -> bool {
    text.match_indices(TASK_PREFIX).any(|(offset, _)| {
        token_start_is_delimited(text, offset)
            && parse_uuid_token(&text[offset + TASK_PREFIX.len()..]).is_some()
    })
}

fn exact_uuid_values(tags: &[String], prefix: &str) -> Vec<Uuid> {
    tags.iter()
        .filter_map(|tag| parse_exact_uuid(tag, prefix))
        .collect()
}

fn parse_exact_uuid(value: &str, prefix: &str) -> Option<Uuid> {
    value.strip_prefix(prefix)?.parse().ok()
}

fn parse_uuid_token(value: &str) -> Option<Uuid> {
    let candidate = value.get(..36)?;
    if value
        .get(36..)
        .and_then(|tail| tail.chars().next())
        .is_some_and(is_token_char)
    {
        return None;
    }
    candidate.parse().ok()
}

fn token_start_is_delimited(text: &str, offset: usize) -> bool {
    text.get(..offset)
        .and_then(|prefix| prefix.chars().next_back())
        .is_none_or(|character| !is_token_char(character))
}

fn is_token_char(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    const TASK_ID: &str = "019fe85e-8e54-7f30-b168-7364a323a812";

    #[test]
    fn direct_plan_tags_require_an_exact_valid_marker() {
        assert!(plan_is_directly_scoped(&[format!("task:{TASK_ID}")]));
        assert!(plan_is_directly_scoped(&[format!(
            "superseded-for-task:{TASK_ID}"
        )]));
        assert!(!plan_is_directly_scoped(&[
            "task:not-a-uuid".to_owned(),
            "task:".to_owned(),
            "task:é50e8400-e29b-41d4-a716-446655440000".to_owned(),
            format!("prefix-task:{TASK_ID}"),
            "superseded-plan".to_owned(),
        ]));
    }

    #[test]
    fn relationship_tags_require_exact_prefixes_and_uuids() {
        let id = Uuid::parse_str(TASK_ID).unwrap();
        let uppercase_id = TASK_ID.to_uppercase();
        assert_eq!(
            superseded_plan_ids(&[format!("supersedes-plan:{TASK_ID}")]),
            vec![id]
        );
        assert_eq!(
            superseded_plan_ids(&[format!("supersedes-plan:{uppercase_id}")]),
            vec![id]
        );
        assert_eq!(
            reviewed_item_ids(&[format!("reviewed-item:{TASK_ID}")]),
            vec![id]
        );
        assert!(superseded_plan_ids(&[format!("x-supersedes-plan:{TASK_ID}")]).is_empty());
        assert!(reviewed_item_ids(&[format!("reviewed-item:{TASK_ID}:extra")]).is_empty());
    }

    #[test]
    fn free_form_task_tokens_require_boundaries_and_valid_uuids() {
        assert!(contains_task_token(&format!(
            "metadata task:{TASK_ID}, done"
        )));
        assert!(contains_task_token(&format!("task:{TASK_ID}")));
        assert!(!contains_task_token(&format!("prefix-task:{TASK_ID}")));
        assert!(!contains_task_token(&format!("task:{TASK_ID}-suffix")));
        assert!(!contains_task_token("a generic task or review"));
        assert!(!contains_task_token("task:not-a-uuid"));
    }
}

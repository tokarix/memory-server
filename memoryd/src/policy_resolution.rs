//! Deterministic resolution of active Rule policies for a trusted execution context.

use std::collections::{BTreeMap, BTreeSet};

use memory_common::policy::{
    CanonicalPolicySet, CanonicalRule, DeliveryClass, PolicyReference, PolicyState,
    ResolutionContext, ResolutionOptions,
};
use serde_json::json;
use uuid::Uuid;

use crate::error::Error;
use crate::model::MemorySummary;
use crate::protocol::RuleList;

const GENERAL: &str = "general";

/// Complete options for one resolution, without inferred execution facts.
#[derive(Clone, Debug)]
pub struct ResolutionRequest {
    /// Project whose policies are being loaded.
    pub project: String,
    /// Include optional general guidance.
    pub include_general: bool,
    /// Permit a well-scoped project policy to replace general guidance.
    pub shadow_general: bool,
    /// ALL-of filter for contextual rules only.
    pub tags: Option<Vec<String>>,
    /// Trusted execution facts.
    pub context: ResolutionContext,
}

fn reference(rule: &MemorySummary) -> Option<PolicyReference> {
    rule.policy.as_ref().map(|policy| PolicyReference {
        project: rule.project.clone(),
        id: rule.id,
        policy_key: policy.policy_key.clone(),
        revision: policy.revision,
        delivery_class: policy.delivery_class,
        selectors: policy.selectors.clone(),
    })
}

fn sort_key(rule: &MemorySummary) -> (u8, &str, &str, i64, Uuid) {
    match &rule.policy {
        Some(policy) => (
            u8::from(policy.delivery_class != DeliveryClass::Mandatory),
            &policy.policy_key,
            &rule.project,
            policy.revision,
            rule.id,
        ),
        None => (2, "", "", 0, rule.id),
    }
}

fn sorted_refs(rules: &[&MemorySummary]) -> Vec<PolicyReference> {
    let mut refs: Vec<_> = rules.iter().filter_map(|rule| reference(rule)).collect();
    refs.sort_by(|a, b| {
        let rank = |r: &PolicyReference| u8::from(r.delivery_class != DeliveryClass::Mandatory);
        (rank(a), &a.policy_key, &a.project, a.revision, a.id).cmp(&(
            rank(b),
            &b.policy_key,
            &b.project,
            b.revision,
            b.id,
        ))
    });
    refs
}

fn failure(
    code: &str,
    message: &str,
    context: &ResolutionContext,
    rules: &[&MemorySummary],
    extra: &serde_json::Value,
    conflict: bool,
) -> Error {
    Error::Policy {
        code: code.to_owned(),
        message: message.to_owned(),
        details: json!({
            "context": context,
            "references": sorted_refs(rules),
            "action": match code {
                "policy_context_required" => "Supply trusted execution context for every listed selector dimension",
                "policy_mandatory_override" => "Use a different policy key and compatible declared values; general mandatory policies remain effective",
                "policy_ambiguous_override" => "Narrow the project selectors to the general policy domain, or use a distinct key",
                "policy_override_disabled" => "Enable safe same-key shadowing or remove the competing project policy",
                "policy_value_conflict" => "Publish compatible successors or separate the policies with disjoint selectors",
                _ => "Inspect and repair the listed policy revisions",
            },
            "detail": extra,
        }),
        conflict,
    }
}

/// Resolve an unfiltered one-snapshot Rule candidate set, or fail without partial output.
///
/// # Errors
/// Returns typed policy diagnostics for missing context and conflicting policies.
#[allow(clippy::too_many_lines)]
pub fn resolve(
    candidates: Vec<MemorySummary>,
    request: &ResolutionRequest,
) -> Result<RuleList, Error> {
    request.context.validate().map_err(|message| Error::Policy {
        code: "policy_context_invalid".to_owned(),
        message: message.to_owned(),
        details: json!({"context": request.context, "action": "Supply canonical context identifiers"}),
        conflict: false,
    })?;

    let mut active: Vec<_> = candidates
        .into_iter()
        .filter(|rule| {
            rule.policy
                .as_ref()
                .is_none_or(|policy| policy.state == PolicyState::Active)
        })
        .collect();
    active.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
    let mut identities: BTreeMap<(&str, &str), &MemorySummary> = BTreeMap::new();
    for rule in &active {
        if let Some(policy) = &rule.policy
            && let Some(previous) = identities.insert((&rule.project, &policy.policy_key), rule)
        {
            return Err(failure(
                "policy_duplicate_active",
                "Multiple active revisions share one project policy identity",
                &request.context,
                &[previous, rule],
                &json!({"project": rule.project, "policy_key": policy.policy_key}),
                true,
            ));
        }
    }

    let participating: Vec<_> = active
        .into_iter()
        .filter(|rule| {
            rule.project == request.project
                || (rule.project == GENERAL
                    && (rule
                        .policy
                        .as_ref()
                        .is_some_and(|p| p.delivery_class == DeliveryClass::Mandatory)
                        || request.include_general))
        })
        .collect();
    let mut applicable = Vec::new();
    let mut missing = BTreeSet::new();
    let mut uncertain = Vec::new();
    for rule in &participating {
        match &rule.policy {
            None => applicable.push(rule),
            Some(policy) => match policy.selectors.missing_for(&request.context) {
                None => {}
                Some(fields) if fields.is_empty() => applicable.push(rule),
                Some(fields) => {
                    missing.extend(fields);
                    uncertain.push(rule);
                }
            },
        }
    }
    if !uncertain.is_empty() {
        return Err(failure(
            "policy_context_required",
            "Execution context is incomplete for applicable policy candidates",
            &request.context,
            &uncertain,
            &json!({"missing_fields": missing}),
            false,
        ));
    }

    let mut overridden: BTreeMap<Uuid, PolicyReference> = BTreeMap::new();
    for general in applicable.iter().filter(|rule| rule.project == GENERAL) {
        let Some(general_policy) = &general.policy else {
            continue;
        };
        if request.project == GENERAL {
            continue;
        }
        for project in applicable
            .iter()
            .filter(|rule| rule.project == request.project)
        {
            let Some(project_policy) = &project.policy else {
                continue;
            };
            if project_policy.policy_key != general_policy.policy_key {
                continue;
            }
            if general_policy.delivery_class == DeliveryClass::Mandatory {
                return Err(failure(
                    "policy_mandatory_override",
                    "A project policy collides with a mandatory general policy",
                    &request.context,
                    &[general, project],
                    &json!({"policy_key": general_policy.policy_key}),
                    true,
                ));
            }
            if !request.shadow_general {
                return Err(failure(
                    "policy_override_disabled",
                    "A same-key project policy requires enabled shadowing",
                    &request.context,
                    &[general, project],
                    &json!({"policy_key": general_policy.policy_key}),
                    true,
                ));
            }
            if !project_policy
                .selectors
                .is_subset_of(&general_policy.selectors)
            {
                return Err(failure(
                    "policy_ambiguous_override",
                    "Project policy selectors do not fit within the general policy domain",
                    &request.context,
                    &[general, project],
                    &json!({"policy_key": general_policy.policy_key}),
                    true,
                ));
            }
            if let Some(general_ref) = reference(general) {
                overridden.insert(project.id, general_ref);
            }
        }
    }

    let winners: Vec<_> = applicable
        .into_iter()
        .filter(|rule| {
            rule.project != GENERAL || !overridden.values().any(|prior| prior.id == rule.id)
        })
        .collect();
    let mut settings: BTreeMap<&str, (&str, &MemorySummary)> = BTreeMap::new();
    for rule in &winners {
        if let Some(policy) = &rule.policy {
            for (key, value) in &policy.values {
                if let Some((old_value, old_rule)) = settings.get(key.as_str()) {
                    if *old_value != value {
                        return Err(failure(
                            "policy_value_conflict",
                            "Effective policies require incompatible values for one setting",
                            &request.context,
                            &[old_rule, rule],
                            &json!({"setting_key": key, "values": [old_value, value]}),
                            true,
                        ));
                    }
                } else {
                    settings.insert(key, (value, rule));
                }
            }
        }
    }

    let mut delivered: Vec<_> = winners
        .into_iter()
        .filter(|rule| {
            rule.policy
                .as_ref()
                .is_some_and(|policy| policy.delivery_class == DeliveryClass::Mandatory)
                || request
                    .tags
                    .as_ref()
                    .is_none_or(|tags| tags.iter().all(|tag| rule.tags.contains(tag)))
        })
        .collect();
    delivered.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
    let canonical_effective: Vec<_> = delivered
        .iter()
        .map(|rule| {
            let policy = rule.policy.as_ref();
            CanonicalRule {
                project: rule.project.clone(),
                id: rule.id,
                policy_key: policy.map(|p| p.policy_key.clone()),
                revision: policy.map(|p| p.revision),
                delivery_class: policy.map(|p| p.delivery_class),
                selectors: policy.map_or_else(Default::default, |p| p.selectors.clone()),
                values: policy.map_or_else(Default::default, |p| p.values.clone()),
                content: rule.content.clone(),
                overrides: overridden.get(&rule.id).cloned(),
            }
        })
        .collect();
    let canonical = CanonicalPolicySet {
        schema_version: 1,
        mandatory: canonical_effective
            .iter()
            .filter(|rule| rule.delivery_class == Some(DeliveryClass::Mandatory))
            .cloned()
            .collect(),
        effective: canonical_effective,
    };
    let (general_rules, project_rules) = delivered
        .into_iter()
        .cloned()
        .partition(|rule| rule.project == GENERAL);
    Ok(RuleList {
        general_rules,
        project_rules,
        canonical,
        context: request.context.clone(),
        options: ResolutionOptions {
            include_general: request.include_general,
            shadow_general: request.shadow_general,
            tags: request.tags.clone(),
        },
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use chrono::{TimeZone, Utc};
    use memory_common::policy::{PolicyMetadata, PolicySelectors};

    use super::*;
    use crate::model::Category;

    fn set(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn rule(project: &str, key: &str, id: u128, class: DeliveryClass) -> MemorySummary {
        let time = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        MemorySummary {
            id: Uuid::from_u128(id),
            category: Category::Rule,
            content: key.to_owned(),
            created_at: time,
            updated_at: time,
            project: project.to_owned(),
            summary: key.to_owned(),
            tags: vec!["visible".to_owned()],
            policy: Some(PolicyMetadata {
                policy_key: key.to_owned(),
                revision: 1,
                delivery_class: class,
                state: PolicyState::Active,
                supersedes: None,
                selectors: PolicySelectors::default(),
                values: BTreeMap::new(),
            }),
        }
    }

    fn request() -> ResolutionRequest {
        ResolutionRequest {
            project: "app".to_owned(),
            include_general: true,
            shadow_general: true,
            tags: None,
            context: ResolutionContext::default(),
        }
    }

    fn code(result: Result<RuleList, Error>) -> String {
        match result {
            Err(Error::Policy { code, .. }) => code,
            _ => panic!("expected policy failure"),
        }
    }

    #[test]
    fn unknown_context_reports_both_storage_profiles() {
        let mut host = rule("general", "storage.host", 1, DeliveryClass::Mandatory);
        host.policy.as_mut().unwrap().selectors.profile = Some(set(&["workstation-host"]));
        let mut ci = rule("general", "storage.ci", 2, DeliveryClass::Mandatory);
        ci.policy.as_mut().unwrap().selectors.profile = Some(set(&["woodpecker-container"]));
        let result = resolve(vec![ci, host], &request());
        match result {
            Err(Error::Policy { code, details, .. }) => {
                assert_eq!(code, "policy_context_required");
                assert_eq!(details["detail"]["missing_fields"], json!(["profile"]));
                assert_eq!(details["references"].as_array().unwrap().len(), 2);
            }
            _ => panic!("expected missing context"),
        }
    }

    #[test]
    fn host_and_container_get_separate_mandatory_rules() {
        let mut host = rule("general", "storage.host", 1, DeliveryClass::Mandatory);
        host.policy.as_mut().unwrap().selectors.profile = Some(set(&["workstation-host"]));
        let mut ci = rule("general", "storage.ci", 2, DeliveryClass::Mandatory);
        ci.policy.as_mut().unwrap().selectors.profile = Some(set(&["woodpecker-container"]));
        for (profile, expected) in [("workstation-host", 1), ("woodpecker-container", 2)] {
            let mut options = request();
            options.context.profile = Some(profile.to_owned());
            options.include_general = false;
            options.tags = Some(vec!["unmatched".to_owned()]);
            let result = resolve(vec![ci.clone(), host.clone()], &options).unwrap();
            assert_eq!(result.canonical.mandatory.len(), 1);
            assert_eq!(result.canonical.mandatory[0].id, Uuid::from_u128(expected));
        }
    }

    #[test]
    fn known_mismatch_short_circuits_unknown_dimension() {
        let mut policy = rule("general", "storage.host", 1, DeliveryClass::Mandatory);
        let selectors = &mut policy.policy.as_mut().unwrap().selectors;
        selectors.profile = Some(set(&["workstation-host"]));
        selectors.language = Some(set(&["rust"]));
        let mut options = request();
        options.context.profile = Some("woodpecker-container".to_owned());
        assert!(
            resolve(vec![policy], &options)
                .unwrap()
                .canonical
                .effective
                .is_empty()
        );
    }

    #[test]
    fn override_requires_containment_and_tracks_provenance() {
        let general = rule("general", "build", 1, DeliveryClass::Contextual);
        let mut project = rule("app", "build", 2, DeliveryClass::Contextual);
        project.policy.as_mut().unwrap().selectors.phase = Some(set(&["implementing"]));
        let mut options = request();
        options.context.phase = Some("implementing".to_owned());
        let result = resolve(vec![project.clone(), general.clone()], &options).unwrap();
        assert_eq!(result.canonical.effective.len(), 1);
        assert_eq!(
            result.canonical.effective[0].overrides.as_ref().unwrap().id,
            general.id
        );
        options.shadow_general = false;
        assert_eq!(
            code(resolve(vec![project, general], &options)),
            "policy_override_disabled"
        );
    }

    #[test]
    fn broader_project_and_mandatory_collisions_fail_before_tags() {
        let mut general = rule("general", "build", 1, DeliveryClass::Contextual);
        general.policy.as_mut().unwrap().selectors.phase = Some(set(&["implementing"]));
        let project = rule("app", "build", 2, DeliveryClass::Contextual);
        let mut options = request();
        options.context.phase = Some("implementing".to_owned());
        options.tags = Some(vec!["unmatched".to_owned()]);
        assert_eq!(
            code(resolve(vec![project.clone(), general.clone()], &options)),
            "policy_ambiguous_override"
        );
        general.policy.as_mut().unwrap().delivery_class = DeliveryClass::Mandatory;
        options.include_general = false;
        assert_eq!(
            code(resolve(vec![project, general], &options)),
            "policy_mandatory_override"
        );
    }

    #[test]
    fn conflicting_values_fail_even_when_contextual_tags_hide_one_rule() {
        let mut mandatory = rule("general", "storage.host", 1, DeliveryClass::Mandatory);
        mandatory
            .policy
            .as_mut()
            .unwrap()
            .values
            .insert("build.target".to_owned(), "disk".to_owned());
        let mut contextual = rule("app", "other", 2, DeliveryClass::Contextual);
        contextual
            .policy
            .as_mut()
            .unwrap()
            .values
            .insert("build.target".to_owned(), "tmp".to_owned());
        let mut options = request();
        options.tags = Some(vec!["unmatched".to_owned()]);
        assert_eq!(
            code(resolve(
                vec![contextual.clone(), mandatory.clone()],
                &options
            )),
            "policy_value_conflict"
        );
        contextual
            .policy
            .as_mut()
            .unwrap()
            .values
            .insert("build.target".to_owned(), "disk".to_owned());
        assert_eq!(
            resolve(vec![contextual, mandatory], &options)
                .unwrap()
                .canonical
                .mandatory
                .len(),
            1
        );
    }

    #[test]
    fn duplicate_active_and_superseded_history() {
        let first = rule("app", "build", 1, DeliveryClass::Contextual);
        let second = rule("app", "build", 2, DeliveryClass::Contextual);
        assert_eq!(
            code(resolve(vec![first.clone(), second], &request())),
            "policy_duplicate_active"
        );
        let mut old = first;
        old.policy.as_mut().unwrap().state = PolicyState::Superseded;
        old.policy.as_mut().unwrap().selectors.profile = Some(set(&["workstation-host"]));
        let current = rule("app", "build", 3, DeliveryClass::Contextual);
        assert_eq!(
            resolve(vec![old, current], &request())
                .unwrap()
                .canonical
                .effective
                .len(),
            1
        );
    }

    #[test]
    fn canonical_and_diagnostics_are_independent_of_input_order() {
        let one = rule("general", "one", 1, DeliveryClass::Mandatory);
        let two = rule("app", "two", 2, DeliveryClass::Contextual);
        let forward = resolve(vec![one.clone(), two.clone()], &request()).unwrap();
        let backward = resolve(vec![two.clone(), one.clone()], &request()).unwrap();
        assert_eq!(forward.canonical, backward.canonical);
        let mut conflict = two;
        conflict.policy.as_mut().unwrap().policy_key = "one".to_owned();
        let left = code(resolve(vec![one.clone(), conflict.clone()], &request()));
        let right = code(resolve(vec![conflict, one], &request()));
        assert_eq!(left, right);
    }
}

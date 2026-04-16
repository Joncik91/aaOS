//! Agent-id prefix disambiguation.
//!
//! CLI subcommands that take AGENT_ID accept any unique prefix of a
//! running agent's UUID. This module resolves the prefix against a list
//! of full UUIDs (fetched via `agent.list`).

#[derive(Debug, PartialEq, Eq)]
pub enum PrefixError {
    NotFound,
    Ambiguous(Vec<String>),
}

pub fn resolve_prefix(prefix: &str, candidates: &[String]) -> Result<String, PrefixError> {
    // Exact match wins unconditionally.
    if let Some(exact) = candidates.iter().find(|c| c.as_str() == prefix) {
        return Ok(exact.clone());
    }
    let hits: Vec<String> = candidates
        .iter()
        .filter(|c| c.starts_with(prefix))
        .cloned()
        .collect();
    match hits.len() {
        0 => Err(PrefixError::NotFound),
        1 => Ok(hits.into_iter().next().unwrap()),
        _ => Err(PrefixError::Ambiguous(hits)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> Vec<String> {
        vec![
            "a3b7c9d2-1111-2222-3333-444455556666".into(),
            "b1c8d7e6-aaaa-bbbb-cccc-dddddddddddd".into(),
        ]
    }

    #[test]
    fn unique_prefix_resolves() {
        let r = resolve_prefix("a3b7", &ids()).unwrap();
        assert_eq!(r, ids()[0]);
    }

    #[test]
    fn exact_match_resolves() {
        let full = ids()[0].clone();
        let r = resolve_prefix(&full, &ids()).unwrap();
        assert_eq!(r, full);
    }

    #[test]
    fn no_match_errors_not_found() {
        let err = resolve_prefix("zzzz", &ids()).unwrap_err();
        assert_eq!(err, PrefixError::NotFound);
    }

    #[test]
    fn ambiguous_prefix_lists_candidates() {
        let two_similar = vec![
            "a3b7c9d2-1111".to_string(),
            "a3b7e5f4-2222".to_string(),
        ];
        let err = resolve_prefix("a3b7", &two_similar).unwrap_err();
        match err {
            PrefixError::Ambiguous(v) => {
                assert_eq!(v.len(), 2);
                assert!(v.contains(&two_similar[0]));
                assert!(v.contains(&two_similar[1]));
            }
            other => panic!("expected Ambiguous, got {:?}", other),
        }
    }

    #[test]
    fn empty_prefix_matches_all_so_ambiguous_with_many() {
        let err = resolve_prefix("", &ids()).unwrap_err();
        match err {
            PrefixError::Ambiguous(v) => assert_eq!(v.len(), 2),
            other => panic!("expected Ambiguous, got {:?}", other),
        }
    }

    #[test]
    fn empty_prefix_with_single_candidate_resolves() {
        let one = vec!["only-one-id".to_string()];
        let r = resolve_prefix("", &one).unwrap();
        assert_eq!(r, "only-one-id");
    }

    #[test]
    fn empty_candidates_errors_not_found() {
        let err = resolve_prefix("anything", &[]).unwrap_err();
        assert_eq!(err, PrefixError::NotFound);
    }
}

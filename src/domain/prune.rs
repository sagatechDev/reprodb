use std::collections::BTreeMap;

use crate::domain::{DatabaseName, DumpId, ProfileName};

/// Retention applied by `cache prune` when no explicit criterion is given.
///
/// This is deliberately unrelated to the pull freshness TTL: freshness decides
/// whether `pull` may reuse a dump, retention decides whether the dump may be
/// destroyed. A dump that is too old for `pull` stays restorable until the user
/// prunes it.
pub const DEFAULT_RETENTION_SECONDS: u64 = 7 * 24 * 60 * 60;

/// Which complete artifacts a prune run destroys.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PruneSelection {
    /// Remove artifacts whose age reached `seconds`.
    OlderThan { seconds: u64 },
    /// Keep the newest `count` artifacts of every `(profile, database)` pair.
    KeepLast { count: usize },
    /// Remove every artifact in scope.
    All,
}

impl Default for PruneSelection {
    fn default() -> Self {
        Self::OlderThan {
            seconds: DEFAULT_RETENTION_SECONDS,
        }
    }
}

/// A complete artifact considered for removal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PruneCandidate {
    pub profile: ProfileName,
    pub database: DatabaseName,
    pub dump_id: DumpId,
    pub completed_at_unix_seconds: u64,
    pub compressed_bytes: u64,
}

/// Decides which candidates a prune run destroys.
///
/// Pure: no filesystem, no locks. The caller is responsible for acquiring the
/// exclusive artifact lock and for skipping artifacts that are leased.
///
/// An artifact completed in the future relative to `now_unix_seconds` is never
/// selected by [`PruneSelection::OlderThan`]; a wrong clock must not destroy a
/// dump that has not aged yet. `KeepLast` and `All` still reach it, because
/// there the user named the artifacts rather than their age.
pub fn select_for_prune(
    selection: PruneSelection,
    candidates: &[PruneCandidate],
    now_unix_seconds: u64,
) -> Vec<PruneCandidate> {
    match selection {
        PruneSelection::All => candidates.to_vec(),
        PruneSelection::OlderThan { seconds } => candidates
            .iter()
            .filter(|candidate| {
                candidate
                    .completed_at_unix_seconds
                    .checked_add(seconds)
                    .is_some_and(|expires_at| {
                        candidate.completed_at_unix_seconds <= now_unix_seconds
                            && now_unix_seconds >= expires_at
                    })
            })
            .cloned()
            .collect(),
        PruneSelection::KeepLast { count } => {
            let mut grouped: BTreeMap<(&ProfileName, &DatabaseName), Vec<&PruneCandidate>> =
                BTreeMap::new();
            for candidate in candidates {
                grouped
                    .entry((&candidate.profile, &candidate.database))
                    .or_default()
                    .push(candidate);
            }
            grouped
                .into_values()
                .flat_map(|mut group| {
                    // Newest first, with the dump ID breaking ties so two dumps
                    // completed within the same second still prune deterministically.
                    group.sort_by(|left, right| {
                        right
                            .completed_at_unix_seconds
                            .cmp(&left.completed_at_unix_seconds)
                            .then_with(|| right.dump_id.cmp(&left.dump_id))
                    });
                    group.into_iter().skip(count).cloned().collect::<Vec<_>>()
                })
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(profile: &str, database: &str, completed_at_unix_seconds: u64) -> PruneCandidate {
        PruneCandidate {
            profile: ProfileName::try_from(profile).unwrap(),
            database: DatabaseName::try_from(database).unwrap(),
            dump_id: DumpId::new(),
            completed_at_unix_seconds,
            compressed_bytes: 1024,
        }
    }

    fn ids(selected: &[PruneCandidate]) -> Vec<DumpId> {
        selected.iter().map(|candidate| candidate.dump_id).collect()
    }

    #[test]
    fn older_than_is_inclusive_at_the_retention_boundary() {
        let exactly_at = candidate("local-source", "acme_production", 1_000);
        let one_second_younger = candidate("local-source", "acme_production", 1_001);
        let candidates = vec![exactly_at.clone(), one_second_younger];

        let selected = select_for_prune(
            PruneSelection::OlderThan { seconds: 100 },
            &candidates,
            1_100,
        );

        assert_eq!(ids(&selected), vec![exactly_at.dump_id]);
    }

    #[test]
    fn older_than_never_selects_an_artifact_completed_in_the_future() {
        let candidates = vec![candidate("local-source", "acme_production", 5_000)];

        let selected = select_for_prune(
            PruneSelection::OlderThan { seconds: 100 },
            &candidates,
            1_000,
        );

        assert!(selected.is_empty());
    }

    #[test]
    fn older_than_saturates_instead_of_overflowing() {
        let candidates = vec![candidate("local-source", "acme_production", 10)];

        let selected = select_for_prune(
            PruneSelection::OlderThan { seconds: u64::MAX },
            &candidates,
            u64::MAX,
        );

        assert!(selected.is_empty());
    }

    #[test]
    fn keep_last_counts_per_profile_and_database_pair() {
        let newest_acme = candidate("local-source", "acme_production", 300);
        let older_acme = candidate("local-source", "acme_production", 200);
        let oldest_acme = candidate("local-source", "acme_production", 100);
        let only_globex = candidate("local-source", "globex_production", 50);
        let staging_acme = candidate("staging", "acme_production", 10);
        let candidates = vec![
            older_acme.clone(),
            newest_acme,
            oldest_acme.clone(),
            only_globex,
            staging_acme,
        ];

        let selected = select_for_prune(PruneSelection::KeepLast { count: 1 }, &candidates, 1_000);

        let mut selected_ids = ids(&selected);
        selected_ids.sort();
        let mut expected = vec![older_acme.dump_id, oldest_acme.dump_id];
        expected.sort();
        assert_eq!(selected_ids, expected);
    }

    #[test]
    fn keep_last_larger_than_the_group_selects_nothing() {
        let candidates = vec![
            candidate("local-source", "acme_production", 200),
            candidate("local-source", "acme_production", 100),
        ];

        let selected = select_for_prune(PruneSelection::KeepLast { count: 5 }, &candidates, 1_000);

        assert!(selected.is_empty());
    }

    #[test]
    fn keep_last_zero_selects_every_candidate() {
        let candidates = vec![
            candidate("local-source", "acme_production", 200),
            candidate("staging", "acme_production", 100),
        ];

        let selected = select_for_prune(PruneSelection::KeepLast { count: 0 }, &candidates, 1_000);

        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn keep_last_breaks_ties_deterministically() {
        let first = candidate("local-source", "acme_production", 100);
        let second = candidate("local-source", "acme_production", 100);
        let candidates = vec![first.clone(), second.clone()];

        let selected = select_for_prune(PruneSelection::KeepLast { count: 1 }, &candidates, 1_000);
        let reversed_input = vec![second.clone(), first.clone()];
        let reversed = select_for_prune(
            PruneSelection::KeepLast { count: 1 },
            &reversed_input,
            1_000,
        );

        assert_eq!(selected.len(), 1);
        assert_eq!(ids(&selected), ids(&reversed));
    }

    #[test]
    fn all_reaches_every_candidate_including_a_future_clock() {
        let candidates = vec![
            candidate("local-source", "acme_production", 100),
            candidate("local-source", "acme_production", 9_000),
        ];

        let selected = select_for_prune(PruneSelection::All, &candidates, 1_000);

        assert_eq!(selected.len(), 2);
    }

    #[test]
    fn the_default_selection_is_the_documented_retention() {
        assert_eq!(
            PruneSelection::default(),
            PruneSelection::OlderThan {
                seconds: DEFAULT_RETENTION_SECONDS
            }
        );
        assert_eq!(DEFAULT_RETENTION_SECONDS, 7 * 24 * 60 * 60);
    }
}

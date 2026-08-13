//! The persisted participant roster and its process-manager keys.

use std::collections::BTreeMap;

use crate::model::process::ProcessKey;
use phoxal_bundle::RuntimeBundle;
use phoxal_model::identity::ComponentInstanceId;
use phoxal_runtime_contract::identity::ParticipantId;
use phoxal_runtime_contract::metadata::ParticipantKind;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RosterEntry {
    pub(crate) key: ProcessKey,
    pub(crate) kind: ParticipantKind,
    pub(crate) component: Option<ComponentInstanceId>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Roster {
    entries: Vec<RosterEntry>,
    by_participant: BTreeMap<ParticipantId, usize>,
}

impl Roster {
    pub(crate) fn from_bundle(bundle: &RuntimeBundle) -> Self {
        let entries: Vec<_> = bundle
            .participants()
            .iter()
            .map(|participant| {
                let kind = bundle.artifacts()[participant.artifact()].contract().kind;
                RosterEntry {
                    key: participant.id().clone().into(),
                    kind,
                    component: participant.component().cloned(),
                }
            })
            .collect();
        let by_participant = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.key.participant().clone(), index))
            .collect();
        Self {
            entries,
            by_participant,
        }
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = &RosterEntry> {
        self.entries.iter()
    }

    pub(crate) fn resolve(&self, participant: &ParticipantId) -> Option<&RosterEntry> {
        self.by_participant
            .get(participant)
            .map(|index| &self.entries[*index])
    }

    #[cfg(test)]
    pub(crate) fn test_fixture() -> Self {
        let participant = ParticipantId::new("brain").expect("fixture participant");
        let entries = vec![RosterEntry {
            key: participant.clone().into(),
            kind: ParticipantKind::Brain,
            component: None,
        }];
        let by_participant = [(participant, 0)].into_iter().collect();
        Self {
            entries,
            by_participant,
        }
    }

    #[cfg(test)]
    pub(crate) fn out_of_order_test_fixture() -> Self {
        let entries = ["webots", "brain"]
            .into_iter()
            .map(|id| RosterEntry {
                key: ParticipantId::new(id).expect("fixture participant").into(),
                kind: ParticipantKind::Service,
                component: None,
            })
            .collect::<Vec<_>>();
        let by_participant = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.key.participant().clone(), index))
            .collect();
        Self {
            entries,
            by_participant,
        }
    }
}

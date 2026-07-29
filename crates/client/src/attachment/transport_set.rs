use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use phoxal::raw::{Bus, BusConfig};
use phoxal_cli_core::identity::ProducerId;
use phoxal_cli_core::runtime::{ProcessScope, RobotKey};
use phoxal_cli_protocol::SupervisorSnapshotV0;
use tokio_util::sync::CancellationToken;

pub(crate) struct GraphTransportSet {
    buses: BTreeMap<RobotKey, Bus>,
}

impl GraphTransportSet {
    pub async fn open(
        snapshot: &SupervisorSnapshotV0,
        cancellation: &CancellationToken,
    ) -> Result<Self> {
        let mut buses: BTreeMap<RobotKey, Bus> = BTreeMap::new();
        for robot in robot_keys(snapshot) {
            let opened = Bus::open(BusConfig {
                namespace: robot.namespace.clone(),
                robot_id: robot.robot_id.clone(),
                participant: "phoxal-cli-attachment".to_string(),
                execution: snapshot.execution_id,
                producer: ProducerId::mint(),
                connect_endpoints: vec![snapshot.router.clone()],
            });
            let bus = match tokio::select! {
                _ = cancellation.cancelled() => Err(anyhow::anyhow!("attachment cancelled")),
                result = opened => result.map_err(Into::into),
            } {
                Ok(bus) => bus,
                Err(error) => {
                    for bus in buses.values() {
                        let _ = bus.close().await;
                    }
                    return Err(error);
                }
            };
            buses.insert(robot, bus);
        }
        Ok(Self { buses })
    }

    pub fn iter(&self) -> impl Iterator<Item = (&RobotKey, &Bus)> {
        self.buses.iter()
    }

    pub async fn close(self) {
        for bus in self.buses.values() {
            if let Err(error) = bus.close().await {
                tracing::debug!(error = %error, "attachment graph bus close failed");
            }
        }
    }
}

pub(crate) fn robot_keys(snapshot: &SupervisorSnapshotV0) -> BTreeSet<RobotKey> {
    snapshot
        .processes
        .keys()
        .filter_map(|key| match &key.scope {
            ProcessScope::Robot(robot) => Some(robot.clone()),
            ProcessScope::Project => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use phoxal_cli_core::runtime::{
        ParticipantKind, ProcessDescriptor, ProcessEntry, ProcessKey, ProcessStatus, RobotKey,
        StartupRequirement,
    };

    use super::*;

    #[test]
    fn one_transport_root_is_selected_for_each_robot() {
        let left = RobotKey::new("lab", "left");
        let right = RobotKey::new("lab", "right");
        let mut snapshot = SupervisorSnapshotV0::default();
        for (robot, id) in [
            (left.clone(), "drive"),
            (left.clone(), "camera"),
            (right.clone(), "drive"),
        ] {
            let key = ProcessKey::robot(robot, id);
            snapshot.processes.insert(
                key.clone(),
                ProcessEntry {
                    descriptor: ProcessDescriptor {
                        key,
                        kind: ParticipantKind::Service,
                        artifact: "drive".into(),
                        owner: "project".into(),
                        startup_requirement: StartupRequirement::Required,
                    },
                    status: ProcessStatus::default(),
                },
            );
        }
        assert_eq!(robot_keys(&snapshot), BTreeSet::from([left, right]));
    }

    #[test]
    fn graph_replacement_selects_only_the_new_snapshot_roots() {
        let mut old = SupervisorSnapshotV0::default();
        let old_robot = RobotKey::new("lab", "old");
        let old_key = ProcessKey::robot(old_robot.clone(), "drive");
        old.processes.insert(
            old_key.clone(),
            ProcessEntry {
                descriptor: ProcessDescriptor {
                    key: old_key,
                    kind: ParticipantKind::Service,
                    artifact: "drive".into(),
                    owner: "project".into(),
                    startup_requirement: StartupRequirement::Required,
                },
                status: ProcessStatus::default(),
            },
        );
        let mut new = old.clone();
        new.graph_generation = 1;
        new.processes.clear();
        let new_robot = RobotKey::new("lab", "new");
        let new_key = ProcessKey::robot(new_robot.clone(), "drive");
        new.processes.insert(
            new_key.clone(),
            ProcessEntry {
                descriptor: ProcessDescriptor {
                    key: new_key,
                    kind: ParticipantKind::Service,
                    artifact: "drive".into(),
                    owner: "project".into(),
                    startup_requirement: StartupRequirement::Required,
                },
                status: ProcessStatus::default(),
            },
        );
        assert_eq!(robot_keys(&old), BTreeSet::from([old_robot]));
        assert_eq!(robot_keys(&new), BTreeSet::from([new_robot]));
    }
}

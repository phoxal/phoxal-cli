use phoxal_bus::zenoh_typed::TypedSchema;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InventoryKind {
    Topic,
    Query,
    Command,
}

#[derive(Debug, Clone)]
pub struct InventoryEntry {
    pub kind: InventoryKind,
    pub path: String,
    pub schema: String,
}

pub fn inventory() -> Vec<InventoryEntry> {
    let mut entries = Vec::new();
    component_topics(&mut entries);
    simulator_topics(&mut entries);
    entries.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.schema.cmp(&right.schema))
    });
    entries
}

fn topic<T: TypedSchema>(path: impl Into<String>) -> InventoryEntry {
    InventoryEntry {
        kind: InventoryKind::Topic,
        path: path.into(),
        schema: T::SCHEMA_NAME.to_string(),
    }
}

fn command_with_ack<Req: TypedSchema, Resp: TypedSchema>(
    path: impl Into<String>,
) -> InventoryEntry {
    InventoryEntry {
        kind: InventoryKind::Command,
        path: path.into(),
        schema: format!("{} -> {}", Req::SCHEMA_NAME, Resp::SCHEMA_NAME),
    }
}

fn component_topics(entries: &mut Vec<InventoryEntry>) {
    use phoxal_component_api::v1::capability as cap;

    const COMPONENT_ID: &str = "{component-id}";
    const CAPABILITY_ID: &str = "{capability-id}";

    entries.extend([
        topic::<cap::accelerometer::Sample>(cap::default_profile_path(COMPONENT_ID, CAPABILITY_ID)),
        topic::<cap::battery::State>(cap::default_profile_path(COMPONENT_ID, CAPABILITY_ID)),
        topic::<cap::camera::Frame>(cap::default_profile_path(COMPONENT_ID, CAPABILITY_ID)),
        topic::<cap::depth::Depth>(cap::default_profile_path(COMPONENT_ID, CAPABILITY_ID)),
        topic::<cap::encoder::Sample>(cap::encoder::topic(COMPONENT_ID, CAPABILITY_ID)),
        topic::<cap::gnss::Sample>(cap::default_profile_path(COMPONENT_ID, CAPABILITY_ID)),
        topic::<cap::gyroscope::Sample>(cap::default_profile_path(COMPONENT_ID, CAPABILITY_ID)),
        topic::<cap::imu::Sample>(cap::imu::topic(COMPONENT_ID, CAPABILITY_ID)),
        topic::<cap::led::Command>(cap::command_path(
            COMPONENT_ID,
            cap::led::KIND,
            CAPABILITY_ID,
        )),
        topic::<cap::lidar::Scan>(cap::default_profile_path(COMPONENT_ID, CAPABILITY_ID)),
        topic::<cap::magnetometer::Sample>(cap::default_profile_path(COMPONENT_ID, CAPABILITY_ID)),
        topic::<cap::microphone::Frame>(cap::default_profile_path(COMPONENT_ID, CAPABILITY_ID)),
        topic::<cap::mmwave::Scan>(cap::default_profile_path(COMPONENT_ID, CAPABILITY_ID)),
        topic::<cap::motor::Command>(cap::motor::topic(COMPONENT_ID, CAPABILITY_ID)),
        topic::<cap::range::Sample>(cap::range::topic(COMPONENT_ID, CAPABILITY_ID)),
        topic::<cap::speaker::audio::Audio>(cap::speaker::audio::path(COMPONENT_ID, CAPABILITY_ID)),
        topic::<cap::speaker::command::Command>(cap::speaker::command::path(
            COMPONENT_ID,
            CAPABILITY_ID,
        )),
    ]);
}

fn simulator_topics(entries: &mut Vec<InventoryEntry>) {
    entries.extend([
        topic::<phoxal_simulator_api::v1::clock::Clock>(phoxal_simulator_api::v1::clock::TOPIC),
        topic::<phoxal_simulator_api::v1::status::Status>(phoxal_simulator_api::v1::status::TOPIC),
        topic::<phoxal_simulator_api::v1::pose::Pose>(phoxal_simulator_api::v1::pose::path(
            "{robot-id}",
        )),
        topic::<phoxal_simulator_api::v1::contact::Contact>(
            phoxal_simulator_api::v1::contact::path("{robot-id}"),
        ),
        topic::<phoxal_simulator_api::v1::collision::Collision>(
            phoxal_simulator_api::v1::collision::path("{robot-id}"),
        ),
        command_with_ack::<
            phoxal_simulator_api::v1::reset::Request,
            phoxal_simulator_api::v1::reset::Response,
        >(phoxal_simulator_api::v1::reset::TOPIC),
    ]);
}

#[cfg(test)]
mod tests {
    use super::{InventoryKind, inventory};
    use phoxal_bus::zenoh_typed::TypedSchema;

    #[test]
    fn camera_and_depth_inventory_use_default_profile_topics() {
        let entries = inventory();

        assert!(entries.iter().any(|entry| {
            entry.kind == InventoryKind::Topic
                && entry.path == "component/{component-id}/{capability-id}/profile/default"
                && entry.schema == phoxal_component_api::v1::capability::camera::Frame::SCHEMA_NAME
        }));
        assert!(entries.iter().any(|entry| {
            entry.kind == InventoryKind::Topic
                && entry.path == "component/{component-id}/{capability-id}/profile/default"
                && entry.schema == phoxal_component_api::v1::capability::depth::Depth::SCHEMA_NAME
        }));
    }

    #[test]
    fn sensor_inventory_uses_default_profile_topics() {
        let entries = inventory();

        assert!(entries.iter().any(|entry| {
            entry.kind == InventoryKind::Topic
                && entry.path == "component/{component-id}/{capability-id}/profile/default"
                && entry.schema == phoxal_component_api::v1::capability::imu::Sample::SCHEMA_NAME
        }));
    }
}

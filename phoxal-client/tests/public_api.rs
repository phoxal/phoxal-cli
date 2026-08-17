//! Public-surface compile contract for an application depending only on
//! `phoxal-client`.

use phoxal_client::{Client, ClientError, robot, supervisor};

#[allow(dead_code)]
async fn every_descriptor_driven_operation(
    client: &Client,
) -> Result<(), Box<dyn std::error::Error>> {
    let _state = client
        .state_view(robot::topic::client().motion().state())
        .await?;
    let _event = client
        .event_receiver(robot::topic::client().navigation().result())
        .await?;
    let _sample = client
        .sample_receiver(
            robot::topic::client()
                .component("chassis")?
                .encoder("left")?
                .sample(),
        )
        .await?;
    let _stream = client
        .stream_receiver(supervisor::topic::client().logs().follow())
        .await?;
    let _setpoint = client.setpoint_publisher(robot::topic::client().motion().manual())?;
    let _input_stream = client.stream_publisher(
        robot::topic::client()
            .component("speaker")?
            .speaker("audio")?
            .stream(),
    )?;
    let _query = client.querier(supervisor::topic::client().command().topic())?;
    Ok(())
}

/// The robot a connection is attached to is one query away, and the answer is
/// the bundle manifest itself: an application that depends only on this crate
/// can name the reply and read the compiled robot out of it.
#[allow(dead_code)]
async fn the_manifest_is_reachable_through_the_client(
    client: &Client,
) -> Result<String, ClientError> {
    let manifest: supervisor::info::InfoReply = client.manifest().await?;
    Ok(manifest.robot().id().to_string())
}

#[test]
fn protocol_families_and_structured_errors_are_reexported() {
    fn cloneable<T: Clone>() {}
    fn structured_error<T: std::error::Error>() {}

    cloneable::<Client>();
    structured_error::<ClientError>();
    assert_eq!(
        supervisor::topic::client().connect().topic().key(),
        "supervisor/connect"
    );
    assert_eq!(
        supervisor::topic::client().info().topic().key(),
        "supervisor/info"
    );
    assert_eq!(
        robot::topic::client().motion().manual().key(),
        "robot/motion/manual"
    );
}

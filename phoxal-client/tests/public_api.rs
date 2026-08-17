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
        robot::topic::client().motion().manual().key(),
        "robot/motion/manual"
    );
}

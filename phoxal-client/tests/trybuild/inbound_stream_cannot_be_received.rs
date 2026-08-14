use phoxal_client::{Client, robot};

async fn receive_inbound_stream(client: &Client) {
    let _ = client
        .stream_receiver(
            robot::topic::owner()
                .component("speaker")
                .unwrap()
                .speaker("audio")
                .unwrap()
                .stream(),
        )
        .await;
}

fn main() {}

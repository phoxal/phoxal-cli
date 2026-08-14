use phoxal_client::{Client, supervisor};

fn publish_outbound_stream(client: &Client) {
    let _ = client.stream_publisher(supervisor::topic::owner().logs().follow());
}

fn main() {}

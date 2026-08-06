use getrandom::fill;
use phoxal_cli_protocol::SupervisorSnapshotV0;

pub(super) fn initial_snapshot() -> SupervisorSnapshotV0 {
    SupervisorSnapshotV0 {
        supervisor_generation: random_nonzero_u64(),
        ..SupervisorSnapshotV0::default()
    }
}

pub(super) fn bounded_text(value: &str) -> String {
    const MAX_SNAPSHOT_TEXT_BYTES: usize = phoxal_cli_protocol::limits::MAX_SNAPSHOT_TEXT_BYTES;
    if value.len() <= MAX_SNAPSHOT_TEXT_BYTES {
        return value.to_string();
    }
    let suffix = "…";
    let mut end = MAX_SNAPSHOT_TEXT_BYTES.saturating_sub(suffix.len());
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}{}", &value[..end], suffix)
}

fn random_nonzero_u64() -> u64 {
    loop {
        let mut bytes = [0_u8; 8];
        fill(&mut bytes).expect("operating system randomness is available");
        let value = u64::from_ne_bytes(bytes);
        if value != 0 {
            return value;
        }
    }
}

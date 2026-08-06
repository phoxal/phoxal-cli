//! The controller inventory: which pads exist, which one is selected, and
//! whether manual authority is on.
//!
//! Every transition here is pure state - no `gilrs`, no bus - so the rules a
//! human would otherwise have to reproduce with hardware (a pad disconnecting
//! under an active selection must stop the robot; an unmapped pad must never
//! look usable) are unit-tested directly.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use phoxal_cli_observation::{
    JoypadDevice, JoypadDeviceStatus, JoypadDevicesSample, ManualDriveUnsupported,
};

/// Cap on remembered pads. Disconnected entries are kept so a pad that
/// reconnects keeps its id, which is what makes the cap necessary.
pub(super) const MAX_DEVICE_REGISTRY: usize = 64;
pub(super) const MAX_SELECT_ID_BYTES: usize = 128;
const MAX_STABLE_ID_SUFFIX_BYTES: usize = 3;

/// One tracked gamepad, keyed by its stable id.
pub(super) struct PadEntry {
    /// The identity the stable id was derived from (uuid-hex, or a
    /// name-derived fallback). This is what re-associates a reconnecting pad
    /// with its previous stable id, since a backend handle is process-local
    /// and not reused across a disconnect/reconnect cycle.
    pub base_id: String,
    /// The current process-local backend handle, if connected. `None` while
    /// the pad is known but not presently plugged in.
    pub handle: Option<PadHandle>,
    pub name: String,
    pub connected: bool,
    /// Whether the backend has a standardized mapping for the controls the
    /// drive preset reads. An enumerated but unmapped pad must never look
    /// usable while silently producing zero commands.
    pub mapped: bool,
}

/// A process-local backend handle, kept opaque so the registry's rules stay
/// testable without a gamepad backend.
pub(super) type PadHandle = gilrs::GamepadId;

/// What a registry transition requires of the caller.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RegistryChange {
    /// The published inventory differs and must be re-observed.
    pub changed: bool,
    /// Authority was revoked while it was live: publish a stop now.
    pub zero_required: bool,
}

/// Authoritative controller inventory, selection and manual authority, plus
/// structural unavailability and the last rejected request (if any).
#[derive(Default)]
pub(super) struct Registry {
    pub entries: HashMap<String, PadEntry>,
    pub device_order: VecDeque<String>,
    pub selected: Option<String>,
    pub enabled: bool,
    pub last_error: Option<String>,
    /// Why this robot cannot be driven manually at all, as a closed reason the
    /// renderer matches on (organization#978).
    pub unsupported: Option<ManualDriveUnsupported>,
}

impl Registry {
    /// The selection's backend handle, if it is currently connected.
    pub fn selected_handle(&self) -> Option<PadHandle> {
        self.entries.get(self.selected.as_ref()?)?.handle
    }

    pub fn sample(&self) -> JoypadDevicesSample {
        let mut available: Vec<JoypadDevice> = self
            .entries
            .iter()
            .map(|(id, entry)| JoypadDevice {
                id: id.clone(),
                name: entry.name.clone(),
                status: if !entry.connected {
                    JoypadDeviceStatus::Disconnected
                } else if entry.mapped {
                    JoypadDeviceStatus::Ready
                } else {
                    JoypadDeviceStatus::Unsupported
                },
            })
            .collect();
        available.sort_by(|left, right| left.id.cmp(&right.id));
        JoypadDevicesSample {
            available: Arc::new(available),
            selected: self.selected.clone(),
            enabled: self.enabled,
            unsupported: self.unsupported,
            last_error: self.last_error.clone(),
        }
    }

    /// Ensure `handle` is represented, reusing a previously known stable id for
    /// the same physical pad when one exists, and return that id.
    pub fn observe(
        &mut self,
        handle: PadHandle,
        name: String,
        uuid: [u8; 16],
        mapped: bool,
    ) -> String {
        if let Some(stable_id) = self.entries.iter().find_map(|(stable_id, entry)| {
            (entry.handle == Some(handle)).then(|| stable_id.clone())
        }) {
            let entry = self
                .entries
                .get_mut(&stable_id)
                .expect("observed controller must still exist");
            entry.name = name;
            entry.connected = true;
            entry.mapped = mapped;
            self.touch(&stable_id);
            return stable_id;
        }

        let base = base_device_id(uuid, &name);

        if let Some(stable_id) = self.entries.iter().find_map(|(stable_id, entry)| {
            (entry.base_id == base && entry.handle.is_none()).then(|| stable_id.clone())
        }) {
            let entry = self
                .entries
                .get_mut(&stable_id)
                .expect("reconnecting controller must still exist");
            entry.handle = Some(handle);
            entry.connected = true;
            entry.mapped = mapped;
            entry.name = name;
            self.touch(&stable_id);
            return stable_id;
        }

        let stable_id = assign_stable_id(&self.entries, &base);
        self.entries.insert(
            stable_id.clone(),
            PadEntry {
                base_id: base,
                handle: Some(handle),
                name,
                connected: true,
                mapped,
            },
        );
        self.touch(&stable_id);
        stable_id
    }

    /// A pad finished connecting: adopt it if nothing usable is selected.
    pub fn connected(&mut self, stable_id: String) -> RegistryChange {
        let zero_required = self.invalidate_unready_selection();
        let mapped = self
            .entries
            .get(&stable_id)
            .is_some_and(|entry| entry.mapped);
        if self.selected.is_none() && mapped {
            self.selected = Some(stable_id);
            self.enabled = false;
        }
        RegistryChange {
            changed: true,
            zero_required,
        }
    }

    pub fn disconnected(&mut self, handle: PadHandle) -> RegistryChange {
        let stable_id = self.entries.iter().find_map(|(stable_id, entry)| {
            (entry.handle == Some(handle)).then(|| stable_id.clone())
        });
        let Some(stable_id) = stable_id else {
            return RegistryChange::default();
        };
        self.disconnect(&stable_id)
    }

    pub fn disconnect(&mut self, stable_id: &str) -> RegistryChange {
        let name = {
            let Some(entry) = self.entries.get_mut(stable_id) else {
                return RegistryChange::default();
            };
            entry.handle = None;
            entry.connected = false;
            entry.name.clone()
        };
        self.touch(stable_id);
        let selected_disconnected = self.selected.as_deref() == Some(stable_id);
        let zero_required = selected_disconnected && self.invalidate_selection();
        tracing::info!(
            device_id = stable_id,
            device_name = %name,
            selection_cleared = selected_disconnected,
            "controller disconnected"
        );
        RegistryChange {
            changed: true,
            zero_required,
        }
    }

    /// The user asked to select a device by id. An unknown or unusable id
    /// populates `last_error`; either way the caller re-observes, and that
    /// re-observation is the acknowledgement.
    pub fn select(&mut self, id: &str) -> bool {
        if id.len() > MAX_SELECT_ID_BYTES {
            let id = truncate_utf8(id, MAX_SELECT_ID_BYTES);
            let failure = format!("device id '{id}…' exceeds the {MAX_SELECT_ID_BYTES}-byte limit");
            tracing::warn!(device_id = id, reason = %failure, "controller selection rejected");
            self.last_error = Some(failure);
            return false;
        }
        let selection_changes = self.selected.as_deref() != Some(id);
        let failure = match self.entries.get(id) {
            Some(entry) if entry.connected && entry.mapped => {
                let zero_required = self.enabled && selection_changes;
                self.selected = Some(id.to_string());
                if selection_changes {
                    self.enabled = false;
                }
                self.last_error = None;
                tracing::info!(device_id = id, "controller selected");
                return zero_required;
            }
            Some(entry) if entry.connected => {
                format!("device '{}' has no compatible control mapping", entry.name)
            }
            Some(_) => format!("device '{id}' is not connected"),
            None => format!("unknown device id '{id}'"),
        };
        let zero_required = !selection_changes && self.invalidate_selection();
        tracing::warn!(device_id = id, reason = %failure, "controller selection rejected");
        self.last_error = Some(failure);
        zero_required
    }

    /// Apply an enable/disable request. Disabling always requests a stop before
    /// movement publication ends.
    pub fn set_enabled(&mut self, enabled: bool) -> bool {
        if !enabled {
            let was_enabled = self.enabled;
            self.enabled = false;
            self.last_error = None;
            return was_enabled;
        }
        if let Some(unsupported) = self.unsupported {
            self.enabled = false;
            self.last_error = None;
            tracing::warn!(reason = %unsupported, "manual input enable rejected");
            return false;
        }
        let Some(selected) = self.selected.as_ref() else {
            self.enabled = false;
            let error = "no controller is selected".to_string();
            tracing::warn!(reason = %error, "manual input enable rejected");
            self.last_error = Some(error);
            return false;
        };
        let ready = self
            .entries
            .get(selected)
            .is_some_and(|entry| entry.connected && entry.mapped);
        if ready {
            self.enabled = true;
            self.last_error = None;
            tracing::info!(device_id = %selected, "manual input enabled");
        } else {
            let error = format!("selected device '{selected}' is not ready");
            self.enabled = false;
            tracing::warn!(reason = %error, "manual input enable rejected");
            self.last_error = Some(error);
        }
        false
    }

    /// Mark every pad not in `seen` as disconnected. They are kept, not
    /// removed: the same pad may come back and must keep its id.
    pub fn retain_seen(&mut self, seen: &HashSet<String>) -> bool {
        for (stable_id, entry) in &mut self.entries {
            if !seen.contains(stable_id) {
                entry.handle = None;
                entry.connected = false;
            }
        }
        let selected_disconnected = self.selected.as_ref().is_some_and(|selected| {
            self.entries
                .get(selected.as_str())
                .is_some_and(|entry| !entry.connected)
        });
        let zero_required = selected_disconnected && self.invalidate_selection();
        self.prune();
        zero_required
    }

    /// Drop a selection that is no longer usable, and otherwise adopt
    /// `fallback` (the first compatible pad) while leaving authority off.
    pub fn reconcile_selection(&mut self, fallback: Option<PadHandle>) -> bool {
        let zero_required = self.invalidate_unready_selection();
        if self.selected.is_none()
            && let Some(fallback) = fallback
            && let Some((stable_id, _)) = self
                .entries
                .iter()
                .find(|(_, entry)| entry.handle == Some(fallback))
        {
            self.selected = Some(stable_id.clone());
            self.enabled = false;
        }
        zero_required
    }

    fn invalidate_unready_selection(&mut self) -> bool {
        let selection_is_ready = self
            .selected
            .as_ref()
            .and_then(|id| self.entries.get(id))
            .is_some_and(|entry| entry.connected && entry.mapped);
        self.selected.is_some() && !selection_is_ready && self.invalidate_selection()
    }

    fn invalidate_selection(&mut self) -> bool {
        if self.selected.take().is_none() {
            return false;
        }
        let was_enabled = self.enabled;
        self.enabled = false;
        was_enabled
    }

    fn touch(&mut self, stable_id: &str) {
        self.device_order.retain(|known| known != stable_id);
        self.device_order.push_back(stable_id.to_string());
        self.prune();
    }

    fn prune(&mut self) {
        while self.entries.len() > MAX_DEVICE_REGISTRY {
            let candidate = self
                .device_order
                .iter()
                .position(|id| {
                    self.selected.as_deref() != Some(id.as_str())
                        && self.entries.get(id).is_some_and(|entry| !entry.connected)
                })
                .or_else(|| {
                    self.device_order.iter().position(|id| {
                        self.selected.as_deref() != Some(id.as_str())
                            && self.entries.contains_key(id)
                    })
                });
            let Some(position) = candidate else {
                break;
            };
            let stable_id = self
                .device_order
                .remove(position)
                .expect("device eviction candidate must exist");
            if let Some(entry) = self.entries.remove(&stable_id) {
                tracing::warn!(
                    device_id = %stable_id,
                    device_name = %entry.name,
                    connected = entry.connected,
                    "controller registry capacity reached; evicting oldest device"
                );
            }
        }
        self.device_order
            .retain(|stable_id| self.entries.contains_key(stable_id));
    }
}

/// Derive a stable id from a pad's identity, never from the process-local
/// backend handle (which is reassigned on every connect and restart). Prefers
/// the hardware uuid, hex-encoded; falls back to a name-derived id for
/// backends that report an all-zero uuid.
fn base_device_id(uuid: [u8; 16], name: &str) -> String {
    if uuid.iter().all(|byte| *byte == 0) {
        let name_budget = MAX_SELECT_ID_BYTES
            .saturating_sub("name:".len())
            .saturating_sub(MAX_STABLE_ID_SUFFIX_BYTES);
        format!("name:{}", truncate_utf8(name, name_budget))
    } else {
        uuid.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

/// Disambiguate a base id against ids already present, appending `#2`, `#3`,
/// … until it is unique - two identical controllers report the same uuid.
///
/// Known limitation: ids are restart-stable for any pad with a distinct
/// uuid or name. Two *physically identical* controllers sharing a uuid - or two
/// zero-uuid pads with the same name - are separated only by observation-order
/// `#N` suffixes, which can swap across a restart or an out-of-order reconnect.
/// Selecting the "wrong twin" is the only consequence (both are the same
/// model). A stronger identity is deferred until a real requirement appears.
fn assign_stable_id(entries: &HashMap<String, PadEntry>, base: &str) -> String {
    if !entries.contains_key(base) {
        return base.to_string();
    }
    let mut suffix = 2u32;
    loop {
        let candidate = format!("{base}#{suffix}");
        if !entries.contains_key(&candidate) {
            return candidate;
        }
        suffix += 1;
    }
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, connected: bool, mapped: bool) -> (String, PadEntry) {
        (
            id.to_string(),
            PadEntry {
                base_id: id.to_string(),
                handle: None,
                name: id.to_string(),
                connected,
                mapped,
            },
        )
    }

    fn registry_with(entries: impl IntoIterator<Item = (String, PadEntry)>) -> Registry {
        let mut registry = Registry::default();
        for (id, pad) in entries {
            registry.entries.insert(id, pad);
        }
        registry
    }

    #[test]
    fn a_nonzero_uuid_is_hex_encoded() {
        let mut uuid = [0u8; 16];
        uuid[0] = 0xde;
        uuid[1] = 0xad;
        assert_eq!(
            base_device_id(uuid, "Pad"),
            format!("dead{}", "0".repeat(28))
        );
    }

    #[test]
    fn a_zero_uuid_falls_back_to_the_name() {
        assert_eq!(base_device_id([0u8; 16], "Generic Pad"), "name:Generic Pad");
    }

    #[test]
    fn a_name_derived_id_leaves_room_for_a_collision_suffix() {
        let base = base_device_id([0u8; 16], &"controller".repeat(64));
        assert!(base.len() <= MAX_SELECT_ID_BYTES - MAX_STABLE_ID_SUFFIX_BYTES);
        let entries = HashMap::from([entry(&base, false, true)]);
        assert!(assign_stable_id(&entries, &base).len() <= MAX_SELECT_ID_BYTES);
    }

    #[test]
    fn colliding_ids_are_disambiguated_in_order() {
        let mut entries = HashMap::from([entry("abc123", true, true)]);
        assert_eq!(assign_stable_id(&entries, "abc123"), "abc123#2");
        let (id, pad) = entry("abc123#2", true, true);
        entries.insert(id, pad);
        assert_eq!(assign_stable_id(&entries, "abc123"), "abc123#3");
        assert_eq!(assign_stable_id(&HashMap::new(), "abc123"), "abc123");
    }

    #[test]
    fn the_registry_is_bounded_and_never_evicts_the_selection() {
        let mut registry = Registry {
            selected: Some("pad-0".to_string()),
            ..Registry::default()
        };
        for index in 0..(MAX_DEVICE_REGISTRY + 8) {
            let id = format!("pad-{index}");
            let (key, pad) = entry(&id, false, true);
            registry.entries.insert(key, pad);
            registry.touch(&id);
        }

        assert_eq!(registry.entries.len(), MAX_DEVICE_REGISTRY);
        assert_eq!(registry.device_order.len(), MAX_DEVICE_REGISTRY);
        assert!(registry.entries.contains_key("pad-0"));
    }

    #[test]
    fn an_unmapped_device_cannot_be_selected_silently() {
        let mut registry = registry_with([entry("pad", true, false)]);
        registry.entries.get_mut("pad").expect("pad").name = "Unknown Pad".to_string();

        registry.select("pad");

        assert!(registry.selected.is_none());
        assert_eq!(
            registry.last_error.as_deref(),
            Some("device 'Unknown Pad' has no compatible control mapping")
        );
    }

    #[test]
    fn disconnecting_the_live_selection_requires_a_stop() {
        let mut registry = registry_with([entry("pad", true, true)]);
        registry.selected = Some("pad".to_string());
        registry.enabled = true;

        assert_eq!(
            registry.disconnect("pad"),
            RegistryChange {
                changed: true,
                zero_required: true,
            }
        );
        assert!(registry.selected.is_none());
        assert!(!registry.enabled);
        assert!(!registry.entries["pad"].connected);
    }

    #[test]
    fn disconnecting_a_disabled_selection_needs_no_stop() {
        let mut registry = registry_with([entry("pad", true, true)]);
        registry.selected = Some("pad".to_string());

        let outcome = registry.disconnect("pad");

        assert!(outcome.changed);
        assert!(!outcome.zero_required, "nothing was moving to stop");
        assert!(registry.selected.is_none());
    }

    #[test]
    fn a_rescan_that_loses_the_selection_disconnects_and_disables_it() {
        let mut registry = registry_with([entry("missing", true, true)]);
        registry.selected = Some("missing".to_string());
        registry.enabled = true;

        assert!(registry.retain_seen(&HashSet::new()));
        assert!(registry.selected.is_none());
        assert!(!registry.enabled);
        assert!(!registry.entries["missing"].connected);
    }

    #[test]
    fn selection_and_authority_are_authoritative() {
        let mut registry = registry_with([entry("pad", true, true)]);

        assert!(!registry.select("pad"));
        assert!(!registry.enabled, "selecting never enables by itself");
        assert!(!registry.set_enabled(true));
        assert!(registry.enabled);
        assert!(
            registry.set_enabled(false),
            "disabling live authority stops"
        );
        assert!(!registry.enabled);
        assert!(
            !registry.set_enabled(false),
            "already disabled: nothing to stop"
        );
    }

    #[test]
    fn enabling_without_a_selection_fails_closed() {
        let mut registry = Registry::default();

        assert!(!registry.set_enabled(true));
        assert!(!registry.enabled);
        assert_eq!(
            registry.last_error.as_deref(),
            Some("no controller is selected")
        );
    }

    #[test]
    fn an_oversized_selection_id_is_bounded_and_rejected() {
        let mut registry = Registry::default();

        assert!(!registry.select(&"x".repeat(MAX_SELECT_ID_BYTES + 10_000)));
        let error = registry.last_error.expect("rejection acknowledgement");
        assert!(error.contains("exceeds"));
        assert!(
            error.len() < MAX_SELECT_ID_BYTES + 80,
            "the rejection must not echo the whole oversized id"
        );
        assert!(registry.selected.is_none());
    }

    #[test]
    fn enabling_a_disconnected_selection_fails_closed() {
        let mut registry = registry_with([entry("pad", false, true)]);
        registry.selected = Some("pad".to_string());

        assert!(!registry.set_enabled(true));
        assert!(!registry.enabled);
        assert_eq!(
            registry.last_error.as_deref(),
            Some("selected device 'pad' is not ready")
        );
    }

    #[test]
    fn changing_a_live_selection_disables_and_requires_a_stop() {
        let mut registry = registry_with([entry("pad-a", true, true), entry("pad-b", true, true)]);
        registry.selected = Some("pad-a".to_string());
        registry.enabled = true;

        assert!(registry.select("pad-b"));
        assert_eq!(registry.selected.as_deref(), Some("pad-b"));
        assert!(!registry.enabled);
    }

    #[test]
    fn a_selection_that_becomes_unsupported_is_dropped_with_a_stop() {
        let mut registry = registry_with([entry("pad", true, false)]);
        registry.selected = Some("pad".to_string());
        registry.enabled = true;

        assert_eq!(
            registry.connected("pad".to_string()),
            RegistryChange {
                changed: true,
                zero_required: true,
            }
        );
        assert!(registry.selected.is_none());
        assert!(!registry.enabled);
        assert!(
            registry.last_error.is_none(),
            "hardware changing under us is not a rejected user action"
        );
    }

    #[test]
    fn structural_unavailability_is_separate_from_a_transient_rejection() {
        let mut registry = Registry {
            selected: Some("pad".to_string()),
            unavailable_reason: Some(
                "manual input requires differential robot kinematics".to_string(),
            ),
            last_error: Some("old transient error".to_string()),
            ..Registry::default()
        };

        assert!(!registry.set_enabled(true));
        assert!(!registry.enabled);
        assert!(registry.last_error.is_none());
        assert_eq!(
            registry.sample().unavailable_reason.as_deref(),
            Some("manual input requires differential robot kinematics")
        );
    }

    #[test]
    fn the_sample_distinguishes_ready_disconnected_and_unsupported() {
        let registry = registry_with([
            entry("ready", true, true),
            entry("disconnected", false, true),
            entry("unsupported", true, false),
        ]);

        let sample = registry.sample();
        // Sorted by id: disconnected, ready, unsupported.
        assert_eq!(sample.available[0].status, JoypadDeviceStatus::Disconnected);
        assert_eq!(sample.available[1].status, JoypadDeviceStatus::Ready);
        assert_eq!(sample.available[2].status, JoypadDeviceStatus::Unsupported);
    }
}

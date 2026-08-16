//! Local gamepad input.
//!
//! The pad is physically attached to the machine running this client. The
//! client reads it directly and publishes `motion::ManualCommand`, the one
//! result that belongs on the bus.
//!
//! That command is normalized intent, not physics: it is a fraction of what
//! the robot was authored to do, and the robot scales it by its own limits. So
//! this module holds no wheel base and no speed limit, and there is no robot
//! fact it must fetch before an operator can drive.
//!
//! [`Joypad`] is the backend-facing half: it owns the `gilrs` handle and
//! translates its events into [`registry`] transitions. The registry itself is
//! hardware-free and holds every rule worth testing.

mod drive;
mod registry;

use std::collections::HashSet;

use gilrs::{Button, EventType, Gamepad, Gilrs};
use phoxal_cli_observation::JoypadDevicesSample;
use phoxal_client::robot as api;

use registry::{PadHandle, Registry, RegistryChange};

/// How many stop commands a revoked authority queues. Manual stop is the one
/// message that must not be lost to a single dropped publish, so it repeats.
pub(crate) const STOP_REPEAT_COUNT: usize = 3;

/// The local gamepad backend and the inventory derived from it.
pub(crate) struct Joypad {
    /// `None` when the backend could not start at all. The registry still
    /// works and reports why through `unavailable_reason`, so the UI shows a
    /// reason instead of an empty list.
    backend: Option<Gilrs>,
    registry: Registry,
}

impl Joypad {
    /// Open the local gamepad backend and take an initial inventory.
    pub(crate) fn open() -> Self {
        let (backend, backend_unavailable) = match Gilrs::new() {
            Ok(backend) => (Some(backend), None),
            Err(error) => {
                tracing::warn!(error = %error, "gamepad backend unavailable; staying idle");
                (None, Some(format!("gamepad backend unavailable: {error}")))
            }
        };
        let mut joypad = Self {
            backend,
            registry: Registry {
                last_error: backend_unavailable,
                ..Registry::default()
            },
        };
        joypad.rescan();
        joypad
    }

    pub(crate) fn sample(&self) -> JoypadDevicesSample {
        self.registry.sample()
    }

    /// Drain pending backend events. Returns what changed for the caller.
    pub(crate) fn poll(&mut self) -> RegistryChange {
        let mut outcome = RegistryChange::default();
        let Some(backend) = self.backend.as_mut() else {
            return outcome;
        };
        while let Some(event) = backend.next_event() {
            backend.update(&event);
            let applied = match event.event {
                EventType::Connected => {
                    let stable_id = observe(backend, &mut self.registry, event.id);
                    self.registry.connected(stable_id)
                }
                EventType::Disconnected => self.registry.disconnected(event.id),
                // Button and axis values are read from the pad's current state
                // when a command is derived, not accumulated from events.
                _ => RegistryChange::default(),
            };
            outcome.changed |= applied.changed;
            outcome.zero_required |= applied.zero_required;
        }
        outcome
    }

    /// Re-enumerate every connected pad, reconciling against what was already
    /// known so a still-connected pad keeps its id and a pad that vanished is
    /// marked disconnected rather than forgotten.
    pub(crate) fn rescan(&mut self) -> bool {
        let Some(backend) = self.backend.as_ref() else {
            return false;
        };
        let mut seen = HashSet::new();
        for (handle, _) in backend.gamepads() {
            seen.insert(observe(backend, &mut self.registry, handle));
        }
        let missing_zero = self.registry.retain_seen(&seen);
        let fallback = backend
            .gamepads()
            .find(|(_, gamepad)| has_compatible_mapping(gamepad))
            .map(|(handle, _)| handle);
        let reconciliation_zero = self.registry.reconcile_selection(fallback);
        missing_zero || reconciliation_zero
    }

    pub(crate) fn select(&mut self, id: &str) -> bool {
        if self.backend.is_none() {
            tracing::warn!(
                device_id = id,
                reason = self
                    .registry
                    .last_error
                    .as_deref()
                    .unwrap_or("controller backend unavailable"),
                "controller selection ignored"
            );
            return false;
        }
        self.registry.select(id)
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) -> bool {
        self.registry.set_enabled(enabled)
    }

    /// The command the selected pad is currently asking for, or `None` when
    /// manual authority is off or nothing usable is selected.
    ///
    /// A selection that turns out to be disconnected is dropped here rather
    /// than silently producing nothing, so the returned change tells the caller
    /// to stop the robot and re-observe.
    pub(crate) fn command(&mut self) -> (Option<api::motion::ManualCommand>, RegistryChange) {
        if !self.registry.enabled {
            return (None, RegistryChange::default());
        }
        let (Some(backend), Some(handle)) =
            (self.backend.as_ref(), self.registry.selected_handle())
        else {
            return (None, RegistryChange::default());
        };
        let gamepad = backend.gamepad(handle);
        if !gamepad.is_connected() {
            let Some(stable_id) = self.registry.selected.clone() else {
                return (None, RegistryChange::default());
            };
            return (None, self.registry.disconnect(&stable_id));
        }
        let command = drive::command_from_shoulders(
            button_value(&gamepad, Button::LeftTrigger),
            button_value(&gamepad, Button::LeftTrigger2),
            button_value(&gamepad, Button::RightTrigger),
            button_value(&gamepad, Button::RightTrigger2),
        );
        (Some(command), RegistryChange::default())
    }
}

/// Record what the backend currently reports for `handle`, returning the pad's
/// stable id.
fn observe(backend: &Gilrs, registry: &mut Registry, handle: PadHandle) -> String {
    let gamepad = backend.gamepad(handle);
    registry.observe(
        handle,
        gamepad.name().to_string(),
        gamepad.uuid(),
        has_compatible_mapping(&gamepad),
    )
}

/// Every control the drive preset reads must be mapped. A pad missing any of
/// them would enumerate as usable while producing only zeros.
fn has_compatible_mapping(gamepad: &Gamepad<'_>) -> bool {
    [
        Button::LeftTrigger,
        Button::LeftTrigger2,
        Button::RightTrigger,
        Button::RightTrigger2,
    ]
    .into_iter()
    .all(|button| gamepad.button_code(button).is_some())
}

fn button_value(gamepad: &Gamepad<'_>, button: Button) -> f32 {
    gamepad
        .button_data(button)
        .map(gilrs::ev::state::ButtonData::value)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether a robot can be driven is the robot's own business now, so the
    /// only thing that can rule manual input out here is the operator's own
    /// hardware: with nothing selected, authority stays off.
    #[test]
    fn manual_authority_cannot_be_enabled_without_a_selected_pad() {
        let mut joypad = Joypad::open();
        assert!(!joypad.set_enabled(true));
        let sample = joypad.sample();
        assert!(!sample.enabled);
        assert!(sample.selected.is_none());
    }

    #[test]
    fn no_command_is_derived_while_authority_is_off() {
        let mut joypad = Joypad::open();
        let (command, change) = joypad.command();
        assert!(command.is_none());
        assert_eq!(change, RegistryChange::default());
    }
}

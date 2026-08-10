//! Local gamepad input.
//!
//! The pad is physically attached to the machine running this client. The
//! client reads it directly and publishes `motion::ManualCommand`, the one
//! result that belongs on the bus.
//!
//! [`Joypad`] is the backend-facing half: it owns the `gilrs` handle and
//! translates its events into [`registry`] transitions. The registry itself is
//! hardware-free and holds every rule worth testing.

mod drive;
pub(crate) mod manual;
mod registry;

use std::collections::HashSet;

use gilrs::{Button, EventType, Gamepad, Gilrs};
use phoxal_api::v0_1 as api;
use phoxal_cli_observation::{JoypadDevicesSample, ManualDriveUnsupported};

use manual::ManualDrive;

pub(crate) use registry::RegistryChange;
use registry::{PadHandle, Registry};

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
    drive: Option<ManualDrive>,
}

impl Joypad {
    /// Open the local gamepad backend and take an initial inventory.
    ///
    /// `drive` is absent when the robot's model cannot support manual input;
    /// `unsupported` then carries the typed reason. The pads are still
    /// enumerated and shown, but no command is ever derived.
    pub fn open(drive: Option<ManualDrive>, unsupported: Option<ManualDriveUnsupported>) -> Self {
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
                unsupported,
                last_error: backend_unavailable,
                ..Registry::default()
            },
            drive,
        };
        joypad.rescan();
        joypad
    }

    pub fn sample(&self) -> JoypadDevicesSample {
        self.registry.sample()
    }

    /// Drain pending backend events. Returns what changed for the caller.
    pub fn poll(&mut self) -> RegistryChange {
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
    pub fn rescan(&mut self) -> bool {
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

    pub fn select(&mut self, id: &str) -> bool {
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

    pub fn set_enabled(&mut self, enabled: bool) -> bool {
        self.registry.set_enabled(enabled)
    }

    /// The command the selected pad is currently asking for, or `None` when
    /// manual authority is off, nothing usable is selected, or this robot has
    /// no manual drive at all.
    ///
    /// A selection that turns out to be disconnected is dropped here rather
    /// than silently producing nothing, so the returned change tells the caller
    /// to stop the robot and re-observe.
    pub fn command(&mut self) -> (Option<api::motion::ManualCommand>, RegistryChange) {
        if !self.registry.enabled {
            return (None, RegistryChange::default());
        }
        let (Some(drive), Some(backend), Some(handle)) = (
            self.drive,
            self.backend.as_ref(),
            self.registry.selected_handle(),
        ) else {
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
            drive,
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

    /// A user whose robot cannot be driven manually still needs to see whether
    /// their pad is even detected, and to be told why in a reason the renderer
    /// matched on rather than a sentence composed elsewhere.
    #[test]
    fn a_robot_without_manual_drive_still_enumerates_pads_and_names_the_reason() {
        let joypad = Joypad::open(None, Some(ManualDriveUnsupported::NoDifferentialBase));
        let sample = joypad.sample();
        assert_eq!(
            sample.unsupported,
            Some(ManualDriveUnsupported::NoDifferentialBase)
        );
        assert!(!sample.enabled);
    }

    #[test]
    fn manual_authority_cannot_be_enabled_without_a_usable_robot() {
        let mut joypad = Joypad::open(None, Some(ManualDriveUnsupported::NoDifferentialBase));
        assert!(!joypad.set_enabled(true));
        assert!(!joypad.sample().enabled);
    }

    #[test]
    fn no_command_is_derived_while_authority_is_off() {
        let mut joypad = Joypad::open(
            Some(ManualDrive {
                wheel_base_m: 0.5,
                side_speed_mps: 1.0,
            }),
            None,
        );
        let (command, change) = joypad.command();
        assert!(command.is_none());
        assert_eq!(change, RegistryChange::default());
    }
}

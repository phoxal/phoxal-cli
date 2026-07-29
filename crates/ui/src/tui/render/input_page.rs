//! Input page rendering responsibilities.

use super::*;

pub(super) fn draw_input(
    frame: &mut Frame,
    theme: Theme,
    state: &AppState,
    model: &SessionViewModel<'_>,
    area: Rect,
) {
    let sections = if area.width >= 84 {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(72), Constraint::Percentage(28)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(6)])
            .split(area)
    };
    let joypad = model.telemetry.joypad.as_ref();
    let joypad_is_stale =
        joypad.is_some_and(|sample| sample.is_stale(model.now, DEFAULT_FRESHNESS_TTL));
    let live_joypad = joypad.filter(|sample| !sample.is_stale(model.now, DEFAULT_FRESHNESS_TTL));
    let selected_id = live_joypad.and_then(|joypad| joypad.value.selected.as_deref());
    let (linear, angular, motion_updated) = model.telemetry.motion.as_ref().map_or_else(
        || ("n/a".to_string(), "n/a".to_string(), "n/a".to_string()),
        |motion| {
            (
                format!("{:.3} m/s", motion.value.linear_x_mps),
                format!("{:.3} rad/s", motion.value.angular_z_radps),
                format!(
                    "{} ago{}",
                    human::duration(model.now.saturating_duration_since(motion.received_at)),
                    if motion.is_stale(model.now, DEFAULT_FRESHNESS_TTL) {
                        " · stale"
                    } else {
                        ""
                    }
                ),
            )
        },
    );
    let overview = vec![
        Line::from("Command"),
        Line::from(format!("linear  {linear}")),
        Line::from(format!("angular {angular}")),
        Line::from(format!("Motion update {motion_updated}")),
    ];
    let devices = live_joypad
        .map(|joypad| joypad.value.available.as_slice())
        .unwrap_or_default();
    let device_width = sections[0].width.saturating_sub(2);
    let items = devices
        .iter()
        .map(|device| {
            let selected = selected_id.is_some_and(|selected| selected == device.id);
            let item = ListItem::new(device_row(device, selected, device_width));
            if selected {
                item.style(color::fg(theme, Role::Accent).add_modifier(Modifier::BOLD))
            } else {
                item
            }
        })
        .collect::<Vec<_>>();
    let items = if items.is_empty() {
        let empty_state = if joypad_is_stale {
            "Controller state stale · waiting for live tool".to_string()
        } else if let Some(reason) =
            live_joypad.and_then(|sample| sample.value.unavailable_reason.as_deref())
        {
            sanitize_and_fit_cell(
                &format!("Input unavailable · {reason}"),
                usize::from(device_width).saturating_sub(2),
            )
        } else {
            "No controllers observed · r to rescan".to_string()
        };
        vec![ListItem::new(empty_state)]
    } else {
        items
    };
    let mut list_state = ListState::default();
    if state.input_cursor < devices.len() {
        list_state.select(Some(state.input_cursor));
    }
    let candidate_is_selected = devices
        .get(state.input_cursor)
        .is_some_and(|device| selected_id.is_some_and(|selected| selected == device.id));
    let devices_title = live_joypad.map_or_else(
        || "Devices · Select / Enable / Disable / Rescan".to_string(),
        |joypad| {
            if let Some(reason) = joypad
                .value
                .unavailable_reason
                .as_deref()
                .filter(|_| !devices.is_empty())
            {
                let omitted = if joypad.value.devices_truncated == 0 {
                    String::new()
                } else {
                    format!(" · +{} omitted", joypad.value.devices_truncated)
                };
                sanitize_and_fit_cell(
                    &format!("Devices · Input unavailable · {reason}{omitted}"),
                    usize::from(device_width),
                )
            } else if joypad.value.devices_truncated == 0 {
                "Devices · Select / Enable / Disable / Rescan".to_string()
            } else {
                format!(
                    "Devices · {} shown · +{} omitted",
                    devices.len(),
                    joypad.value.devices_truncated
                )
            }
        },
    );
    frame.render_stateful_widget(
        List::new(items)
            .block(shell_block(theme, &devices_title))
            .highlight_style(if candidate_is_selected {
                color::selected(theme, Role::Accent)
            } else {
                color::candidate(theme, Role::Accent)
            })
            .highlight_symbol("› ")
            .highlight_spacing(HighlightSpacing::Always),
        sections[0],
        &mut list_state,
    );
    frame.render_widget(
        Paragraph::new(overview)
            .block(shell_block(theme, "Motion"))
            .wrap(Wrap { trim: true }),
        sections[1],
    );
}

pub(super) fn short_device_id(id: &str) -> String {
    if id.chars().count() <= 14 {
        return id.to_string();
    }
    let prefix = id.chars().take(7).collect::<String>();
    let suffix = id
        .chars()
        .rev()
        .take(5)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{prefix}…{suffix}")
}

pub(super) fn device_row(
    device: &phoxal_cli_core::session::JoypadDevice,
    selected: bool,
    inner_width: u16,
) -> String {
    let available = usize::from(inner_width).saturating_sub(2);
    let status = device_status_label(device.status);
    if available < 60 {
        let name_width = available.saturating_sub(status.len() + 5).max(1);
        let name = sanitize_and_fit_cell(&device.name, name_width);
        return format!("{} {name} · {status}", if selected { "●" } else { "○" });
    }
    let id = sanitize_and_fit_cell(&short_device_id(&device.id), 16);
    let name = sanitize_and_fit_cell(&device.name, 28);
    format!("{} {id} {name} {status}", if selected { "●" } else { "○" })
}

pub(super) fn device_status_label(status: JoypadDeviceStatus) -> &'static str {
    match status {
        JoypadDeviceStatus::Ready => "Ready",
        JoypadDeviceStatus::Disconnected => "Disconnected",
        JoypadDeviceStatus::Unsupported => "Unsupported",
        JoypadDeviceStatus::Unknown => "Unknown",
    }
}

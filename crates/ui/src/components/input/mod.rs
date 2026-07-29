mod model;

pub use model::InputModel;

use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::{Constraint, Direction, Layout, Rect};
use tuirealm::ratatui::widgets::{Paragraph, Row, Table};

use crate::Theme;
use crate::app::{AppModel, InputPanelId, PanelId};
use crate::components::shared::panel_block;

pub fn render(frame: &mut Frame, area: Rect, model: &AppModel, theme: Theme) {
    let [devices, motion] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .areas(area);
    let observation = model.input.observation.as_ref();
    let input_stale =
        model.overview.freshness.get("input") == Some(&phoxal_cli_observation::Freshness::Stale);
    let rows = observation
        .into_iter()
        .flat_map(|observation| observation.joypads.available.iter())
        .map(|device| {
            let candidate = model
                .input
                .candidate
                .as_ref()
                .is_some_and(|id| id.0 == device.id);
            let selected = observation.and_then(|value| value.joypads.selected.as_deref())
                == Some(device.id.as_str());
            Row::new(vec![
                if candidate { ">" } else { " " }.to_string(),
                if selected { "*" } else { " " }.to_string(),
                device.name.clone(),
                device.id.clone(),
                format!("{:?}", device.status).to_lowercase(),
            ])
        });
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Percentage(35),
                Constraint::Percentage(35),
                Constraint::Length(12),
            ],
        )
        .header(Row::new([
            "",
            "",
            "device",
            "id",
            if input_stale {
                "status (stale)"
            } else {
                "status"
            },
        ]))
        .block(panel_block(
            model,
            PanelId::Input(InputPanelId::Devices),
            "Devices (> candidate, * selected)",
            theme,
        )),
        devices,
    );
    let motion_text = observation.and_then(|value| value.motion).map_or_else(
        || "No motion sample".to_string(),
        |motion| {
            format!(
                "linear x: {:.3} m/s\nangular z: {:.3} rad/s",
                motion.linear_x_mps, motion.angular_z_radps
            )
        },
    );
    frame.render_widget(
        Paragraph::new(motion_text).block(panel_block(
            model,
            PanelId::Input(InputPanelId::Motion),
            "Motion",
            theme,
        )),
        motion,
    );
}

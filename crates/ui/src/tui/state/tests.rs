//! Tests for this module.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use crossterm::event::{KeyEventKind, KeyEventState};

use super::*;
use crate::tui::log_view::LogView;
use crate::tui::runtime_view::RuntimeView;
use phoxal_cli_core::session::ParticipantKind;
use phoxal_cli_core::session::RobotScope;

#[test]
fn non_ascii_filters_match_case_insensitively() {
    assert!(CaseInsensitiveNeedle::new("är").contains("Ärger"));
    assert!(CaseInsensitiveNeedle::new("café").contains("CAFÉ"));
    assert!(CaseInsensitiveNeedle::new("σ").contains("ΟΔΟΣ"));
}
use phoxal_cli_core::session::Timestamped;
use phoxal_cli_core::session::{
    BoardSnapshot, LogSource, ParticipantState, ParticipantStatus, RoutedLogLine,
};
use phoxal_cli_core::session::{
    JoypadDevice, JoypadDeviceStatus, JoypadDevicesSample, RuntimeBufferKind, RuntimeDirection,
    RuntimePerformanceSample, RuntimeTopicSample, TelemetrySnapshot,
};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn with_model<T>(telemetry: &TelemetrySnapshot, run: impl FnOnce(&SessionViewModel<'_>) -> T) -> T {
    let board = BoardSnapshot::default();
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let model = SessionViewModel::new(&board, &logs, &runtime, telemetry, Instant::now());
    run(&model)
}

#[test]
fn tabs_preview_with_arrows_and_activate_with_enter() {
    let mut state = AppState::default();
    with_model(&TelemetrySnapshot::default(), |model| {
        for expected in [Page::Runtimes, Page::Logs, Page::Bus, Page::Input] {
            state.handle_key(key(KeyCode::Right), model);
            assert_eq!(state.tab_cursor, expected.index());
            state.handle_key(key(KeyCode::Enter), model);
            assert_eq!(state.page, expected);
            assert_eq!(state.navigation, NavigationLevel::Page);
            state.handle_key(key(KeyCode::Esc), model);
            assert_eq!(state.navigation, NavigationLevel::Tabs);
            assert_eq!(state.tab_cursor, expected.index());
        }
    });
    assert_eq!(
        Page::ALL.map(Page::label),
        ["Overview", "Runtimes", "Logs", "Bus", "Input"]
    );
}

#[test]
fn first_input_attachment_requests_rescan() {
    let mut state = AppState::default();
    with_model(&TelemetrySnapshot::default(), |model| {
        assert_eq!(
            state.handle_key(key(KeyCode::Char('5')), model),
            DisplayAction::JoypadRescan
        );
    });
}

#[test]
fn selection_and_enable_are_separate_authoritative_actions() {
    let now = Instant::now();
    let telemetry = TelemetrySnapshot {
        joypad: Some(Timestamped {
            received_at: now,
            value: JoypadDevicesSample {
                available: vec![JoypadDevice {
                    id: "pad".to_string(),
                    name: "Pad".to_string(),
                    status: JoypadDeviceStatus::Ready,
                }]
                .into(),
                devices_truncated: 0,
                selected: None,
                enabled: false,
                unavailable_reason: None,
                last_error: None,
            },
        }),
        ..TelemetrySnapshot::default()
    };
    let mut state = AppState {
        page: Page::Input,
        navigation: NavigationLevel::Page,
        ..AppState::default()
    };
    with_model(&telemetry, |model| {
        assert_eq!(
            state.handle_key(key(KeyCode::Enter), model),
            DisplayAction::JoypadSelect("pad".to_string())
        );
        assert_eq!(
            state.handle_key(key(KeyCode::Char('e')), model),
            DisplayAction::JoypadSetEnabled(true)
        );
    });
    assert!(!telemetry.joypad.unwrap().value.enabled);
}

#[test]
fn disappearing_input_device_does_not_retarget_selection() {
    let now = Instant::now();
    let devices = |ids: &[&str]| TelemetrySnapshot {
        joypad: Some(Timestamped {
            received_at: now,
            value: JoypadDevicesSample {
                available: ids
                    .iter()
                    .map(|id| JoypadDevice {
                        id: (*id).to_string(),
                        name: (*id).to_string(),
                        status: JoypadDeviceStatus::Ready,
                    })
                    .collect::<Vec<_>>()
                    .into(),
                ..JoypadDevicesSample::default()
            },
        }),
        ..TelemetrySnapshot::default()
    };
    let mut state = AppState {
        page: Page::Input,
        navigation: NavigationLevel::Page,
        ..AppState::default()
    };
    let both = devices(&["pad-a", "pad-b"]);
    with_model(&both, |model| {
        state.sync(model);
        state.handle_key(key(KeyCode::Down), model);
        assert_eq!(
            state.handle_key(key(KeyCode::Enter), model),
            DisplayAction::JoypadSelect("pad-b".to_string())
        );
    });

    let only_a = devices(&["pad-a"]);
    with_model(&only_a, |model| {
        state.sync(model);
        assert_eq!(state.input_cursor, 1);
        assert_eq!(
            state.handle_key(key(KeyCode::Enter), model),
            DisplayAction::None
        );
        state.handle_key(key(KeyCode::Up), model);
        assert_eq!(
            state.handle_key(key(KeyCode::Enter), model),
            DisplayAction::JoypadSelect("pad-a".to_string())
        );
    });
}

#[test]
fn stale_input_device_cannot_be_selected() {
    let telemetry = TelemetrySnapshot {
        joypad: Some(Timestamped {
            received_at: Instant::now() - DEFAULT_FRESHNESS_TTL - std::time::Duration::from_secs(1),
            value: JoypadDevicesSample {
                available: vec![JoypadDevice {
                    id: "stale-pad".to_string(),
                    name: "Stale Pad".to_string(),
                    status: JoypadDeviceStatus::Ready,
                }]
                .into(),
                ..JoypadDevicesSample::default()
            },
        }),
        ..TelemetrySnapshot::default()
    };
    let mut state = AppState {
        page: Page::Input,
        navigation: NavigationLevel::Page,
        ..AppState::default()
    };
    with_model(&telemetry, |model| {
        state.sync(model);
        assert!(state.input_cursor_id.is_none());
        assert_eq!(
            state.handle_key(key(KeyCode::Enter), model),
            DisplayAction::None
        );
    });
}

#[test]
fn logs_keep_independent_filters_severity_and_follow_state() {
    let mut state = AppState {
        page: Page::Logs,
        navigation: NavigationLevel::Page,
        ..AppState::default()
    };
    with_model(&TelemetrySnapshot::default(), |model| {
        state.handle_key(key(KeyCode::Char('s')), model);
        assert_eq!(state.log_severity, SeverityFilter::Error);
        assert_eq!(state.log_filter_cursor, 2);
        state.handle_key(key(KeyCode::Char(' ')), model);
        assert!(!state.log_follow);
        assert_eq!(state.log_filter_cursor, 4);
        state.handle_key(key(KeyCode::Char('/')), model);
        assert_eq!(state.log_filter_cursor, 3);
        for character in ['f', 'a', 'i', 'l'] {
            state.handle_key(key(KeyCode::Char(character)), model);
        }
        state.handle_key(key(KeyCode::Enter), model);
        state.handle_key(key(KeyCode::Char('f')), model);
        assert_eq!(state.log_filter_cursor, 1);
        state.handle_key(key(KeyCode::Esc), model);
    });
    assert_eq!(state.log_text_filter, "fail");
    assert!(state.log_runtime_filter.is_empty());
}

#[test]
fn paused_logs_exclude_lines_with_event_time_after_the_pause_anchor() {
    let started = Instant::now();
    let before_time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
    let mut logs = LogView::new();
    logs.record_at(
        RoutedLogLine {
            participant: "motion".to_string(),
            source: LogSource::Bus,
            severity: LogSeverity::Info,
            text: "before pause".to_string(),
            event_time: before_time,
            scope: None,
        },
        started,
    );
    let board = BoardSnapshot::default();
    let runtime = RuntimeView::new();
    let telemetry = TelemetrySnapshot::default();
    let mut state = AppState {
        page: Page::Logs,
        navigation: NavigationLevel::Page,
        ..AppState::default()
    };
    {
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, started);
        state.handle_key(key(KeyCode::Char(' ')), &model);
        assert_eq!(state.log_pause_anchor, Some(before_time));
        assert_eq!(state.filtered_log_count(&model), 1);
    }

    logs.record_at(
        RoutedLogLine {
            participant: "motion".to_string(),
            source: LogSource::Bus,
            severity: LogSeverity::Info,
            text: "after pause".to_string(),
            event_time: before_time + std::time::Duration::from_secs(1),
            scope: None,
        },
        started + std::time::Duration::from_secs(1),
    );
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, started);
    assert_eq!(state.filtered_log_count(&model), 1);
    assert!(!state.log_follow);
}

#[test]
fn pausing_an_empty_log_view_freezes_before_the_first_matching_line() {
    let started = Instant::now();
    let board = BoardSnapshot::default();
    let mut logs = LogView::new();
    let runtime = RuntimeView::new();
    let telemetry = TelemetrySnapshot::default();
    let mut state = AppState {
        page: Page::Logs,
        navigation: NavigationLevel::Page,
        ..AppState::default()
    };
    {
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, started);
        state.handle_key(key(KeyCode::Char(' ')), &model);
    }
    let anchor = state
        .log_pause_anchor
        .expect("paused empty view still needs an event-time fallback");
    logs.record_at(
        RoutedLogLine {
            participant: "motion".to_string(),
            source: LogSource::Bus,
            severity: LogSeverity::Info,
            text: "first line after pause".to_string(),
            event_time: anchor + std::time::Duration::from_secs(1),
            scope: None,
        },
        started + std::time::Duration::from_secs(1),
    );
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, started);
    assert_eq!(state.filtered_log_count(&model), 0);
    assert!(!state.log_follow);
}

#[test]
fn identical_bus_snapshot_replacement_preserves_paused_visibility() {
    let started = Instant::now();
    let event_time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(7);
    let scope = phoxal_cli_core::session::LogScope {
        namespace: "acme".to_string(),
        robot_id: "r1".to_string(),
    };
    let retained = || RoutedLogLine {
        participant: "motion".to_string(),
        source: LogSource::Bus,
        severity: LogSeverity::Info,
        text: "retained".to_string(),
        event_time,
        scope: Some(scope.clone()),
    };
    let mut logs = LogView::new();
    logs.replace_all(vec![retained()]);
    let board = BoardSnapshot::default();
    let runtime = RuntimeView::new();
    let telemetry = TelemetrySnapshot::default();
    let mut state = AppState {
        page: Page::Logs,
        navigation: NavigationLevel::Page,
        ..AppState::default()
    };

    {
        let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, started);
        state.handle_key(key(KeyCode::Char(' ')), &model);
        assert_eq!(state.log_pause_anchor, Some(event_time));
        assert_eq!(state.filtered_log_count(&model), 1);
    }
    logs.replace_all(vec![retained()]);
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, started);
    assert_eq!(state.filtered_log_count(&model), 1);
}

#[test]
fn runtime_log_shortcut_uses_the_global_log_store_filter() {
    let mut board = BoardSnapshot::default();
    board.participants.insert(
        "motion".to_string(),
        ParticipantStatus::new("motion", ParticipantKind::Service, ParticipantState::Ready),
    );
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let telemetry = TelemetrySnapshot::default();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
    let mut state = AppState {
        page: Page::Runtimes,
        navigation: NavigationLevel::Page,
        log_source_filter: LogSourceFilter::Tools,
        log_text_filter: "old".to_string(),
        log_severity: SeverityFilter::Warn,
        ..AppState::default()
    };
    state.handle_key(key(KeyCode::Char('l')), &model);
    assert_eq!(state.page, Page::Logs);
    assert_eq!(state.log_runtime_filter, "motion");
    assert_eq!(state.log_source_filter, LogSourceFilter::Runtimes);
    assert!(state.log_text_filter.is_empty());
    assert_eq!(state.log_severity, SeverityFilter::All);
}

#[test]
fn bus_sort_filter_and_internal_toggle_are_page_local() {
    let mut state = AppState {
        page: Page::Bus,
        navigation: NavigationLevel::Page,
        ..AppState::default()
    };
    with_model(&TelemetrySnapshot::default(), |model| {
        state.handle_key(key(KeyCode::Char('s')), model);
        assert_eq!(state.bus_sort, BusSort::Topic);
        assert_eq!(state.bus_control_cursor, 1);
        state.handle_key(key(KeyCode::Char('a')), model);
        assert!(state.bus_show_internal);
        assert_eq!(state.bus_control_cursor, 2);
        state.handle_key(key(KeyCode::Char('/')), model);
        assert_eq!(state.bus_control_cursor, 0);
        for character in ['r', 'o', 'u', 't', 'e', 'r'] {
            state.handle_key(key(KeyCode::Char(character)), model);
        }
        state.handle_key(key(KeyCode::Enter), model);
    });
    assert_eq!(state.bus_filter, "router");
    assert!(state.log_text_filter.is_empty());
}

#[test]
fn bus_filter_does_not_bypass_internal_visibility() {
    let telemetry = TelemetrySnapshot {
        router: Some(Timestamped {
            received_at: Instant::now(),
            value: phoxal_cli_core::session::RouterMetricsSample {
                topics: vec![phoxal_cli_core::session::TopicMetric {
                    topic: "dev/robots/rover/v2/router/metrics".to_string(),
                    from_participant: "tool-router".to_string(),
                    ingress_rate_hz: 1.0,
                    count: 1,
                    aggregate_overflow: false,
                }]
                .into(),
                ..phoxal_cli_core::session::RouterMetricsSample::default()
            },
        }),
        ..TelemetrySnapshot::default()
    };
    let mut state = AppState {
        bus_filter: "router".to_string(),
        ..AppState::default()
    };
    with_model(&telemetry, |model| {
        assert_eq!(state.filtered_bus_count(model), 0);
        state.bus_show_internal = true;
        assert_eq!(state.filtered_bus_count(model), 1);
    });
}

#[test]
fn bus_overflow_stays_visible_and_filters_match_sanitized_cells() {
    let telemetry = TelemetrySnapshot {
        router: Some(Timestamped {
            received_at: Instant::now(),
            value: phoxal_cli_core::session::RouterMetricsSample {
                topics: vec![
                    phoxal_cli_core::session::TopicMetric {
                        topic: "drive/state".to_string(),
                        from_participant: "motion".to_string(),
                        ingress_rate_hz: 1.0,
                        count: 1,
                        aggregate_overflow: false,
                    },
                    phoxal_cli_core::session::TopicMetric {
                        topic: "Other/unobserved traffic".to_string(),
                        from_participant: "multiple".to_string(),
                        ingress_rate_hz: 2.0,
                        count: 2,
                        aggregate_overflow: true,
                    },
                ]
                .into(),
                ..phoxal_cli_core::session::RouterMetricsSample::default()
            },
        }),
        ..TelemetrySnapshot::default()
    };
    let mut state = AppState {
        bus_filter: "drive/state".to_string(),
        ..AppState::default()
    };
    with_model(&telemetry, |model| {
        assert_eq!(state.filtered_bus_count(model), 1);
        state.bus_filter.clear();
        assert_eq!(state.filtered_bus_count(model), 2);
        state.bus_show_internal = true;
        assert_eq!(state.filtered_bus_count(model), 2);
    });
}

#[test]
fn bus_arrows_move_the_topic_window_in_reading_order() {
    let mut state = AppState {
        page: Page::Bus,
        navigation: NavigationLevel::Page,
        bus_scroll: 1,
        ..AppState::default()
    };
    with_model(&TelemetrySnapshot::default(), |model| {
        state.handle_key(key(KeyCode::Up), model);
        assert_eq!(state.bus_scroll, 0);
        state.handle_key(key(KeyCode::Down), model);
        assert_eq!(state.bus_scroll, 1);
    });
}

#[test]
fn error_shortcut_exits_editing_and_reveals_the_new_error() {
    let mut state = AppState {
        page: Page::Bus,
        navigation: NavigationLevel::Page,
        runtime_detail_id: Some("motion".to_string()),
        log_severity: SeverityFilter::Warn,
        log_source_filter: LogSourceFilter::Tools,
        log_runtime_filter: "motion".to_string(),
        log_text_filter: "old".to_string(),
        ..AppState::default()
    };
    with_model(&TelemetrySnapshot::default(), |model| {
        state.handle_key(key(KeyCode::Char('/')), model);
        assert_eq!(state.editing, Some(Editing::Bus));
        state.show_help = true;
        state.open_logs_for_error();
        assert_eq!(state.page, Page::Logs);
        assert_eq!(state.editing, None);
        assert!(state.runtime_detail_id.is_none());
        assert!(!state.show_help);
        assert_eq!(state.log_severity, SeverityFilter::Error);
        assert_eq!(state.log_source_filter, LogSourceFilter::All);
        assert!(state.log_runtime_filter.is_empty());
        assert!(state.log_text_filter.is_empty());
        state.page = Page::Bus;
        state.handle_key(key(KeyCode::Char('/')), model);
        assert_eq!(state.editing, Some(Editing::Bus));
        state.open_logs_for_error();
        assert_eq!(state.page, Page::Bus);
        assert_eq!(state.editing, Some(Editing::Bus));
        state.handle_key(key(KeyCode::Char('x')), model);
    });
    assert_eq!(state.bus_filter, "x");

    let mut default_state = AppState::default();
    default_state.open_logs_for_error();
    assert_eq!(default_state.log_severity, SeverityFilter::Error);
}

#[test]
fn sync_leaves_page_window_clamping_to_the_renderer() {
    let mut logs = LogView::new();
    logs.record(phoxal_cli_core::session::RoutedLogLine {
        participant: "motion".to_string(),
        source: phoxal_cli_core::session::LogSource::Bus,
        severity: LogSeverity::Info,
        text: "ready".to_string(),
        event_time: std::time::SystemTime::UNIX_EPOCH,
        scope: None,
    });
    let board = BoardSnapshot::default();
    let runtime = RuntimeView::new();
    let now = Instant::now();
    let telemetry = TelemetrySnapshot {
        router: Some(Timestamped {
            received_at: now,
            value: phoxal_cli_core::session::RouterMetricsSample {
                topics: vec![phoxal_cli_core::session::TopicMetric {
                    topic: "v1/motion/state".to_string(),
                    from_participant: "motion".to_string(),
                    ingress_rate_hz: 1.0,
                    count: 1,
                    aggregate_overflow: false,
                }]
                .into(),
                topics_truncated: 0,
                throughput_msg_s: 1.0,
                window_ns: 1,
            },
        }),
        ..TelemetrySnapshot::default()
    };
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, now);
    let mut state = AppState {
        log_follow: false,
        log_scroll: usize::MAX,
        bus_scroll: usize::MAX,
        ..AppState::default()
    };
    state.sync(&model);
    assert_eq!(state.log_scroll, usize::MAX);
    assert_eq!(state.bus_scroll, usize::MAX);
    state.log_follow = true;
    state.sync(&model);
    assert_eq!(state.log_scroll, 0);
}

#[test]
fn invalid_tab_cursor_falls_back_to_the_active_page() {
    let mut state = AppState {
        page: Page::Logs,
        tab_cursor: usize::MAX,
        navigation: NavigationLevel::Tabs,
        ..AppState::default()
    };
    with_model(&TelemetrySnapshot::default(), |model| {
        state.handle_key(key(KeyCode::Right), model);
        assert_eq!(state.tab_cursor, Page::Bus.index());
        state.tab_cursor = usize::MAX;
        state.handle_key(key(KeyCode::Enter), model);
        assert_eq!(state.page, Page::Logs);
    });
}

#[test]
fn runtime_detail_requires_escape_before_arrows_change_runtime() {
    let mut board = BoardSnapshot::default();
    for id in ["alpha", "beta"] {
        board.participants.insert(
            id.to_string(),
            ParticipantStatus::new(id, ParticipantKind::Service, ParticipantState::Ready),
        );
    }
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let telemetry = TelemetrySnapshot::default();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
    let mut state = AppState {
        page: Page::Runtimes,
        navigation: NavigationLevel::Page,
        ..AppState::default()
    };
    state.handle_key(key(KeyCode::Enter), &model);
    assert_eq!(state.runtime_detail_id.as_deref(), Some("alpha"));
    state.handle_key(key(KeyCode::Down), &model);
    assert_eq!(state.runtime_cursor, 0);
    assert_eq!(state.runtime_detail_id.as_deref(), Some("alpha"));
    state.handle_key(key(KeyCode::Esc), &model);
    state.handle_key(key(KeyCode::Down), &model);
    assert_eq!(state.runtime_cursor, 1);
}

#[test]
fn runtime_cursor_tracks_identity_across_live_state_resorting() {
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let telemetry = TelemetrySnapshot::default();
    let mut initial_board = BoardSnapshot::default();
    for id in ["alpha", "beta"] {
        initial_board.participants.insert(
            id.to_string(),
            ParticipantStatus::new(id, ParticipantKind::Service, ParticipantState::Ready),
        );
    }
    let mut state = AppState {
        page: Page::Runtimes,
        navigation: NavigationLevel::Page,
        ..AppState::default()
    };
    let initial =
        SessionViewModel::new(&initial_board, &logs, &runtime, &telemetry, Instant::now());
    state.sync(&initial);
    state.handle_key(key(KeyCode::Down), &initial);
    assert_eq!(
        state.handle_key(key(KeyCode::Char('r')), &initial),
        DisplayAction::Restart("beta".to_string())
    );

    let mut resorted_board = initial_board;
    resorted_board
        .participants
        .get_mut("beta")
        .expect("beta")
        .state = ParticipantState::Failed;
    let resorted =
        SessionViewModel::new(&resorted_board, &logs, &runtime, &telemetry, Instant::now());
    state.sync(&resorted);
    assert_eq!(state.runtime_cursor, 0);
    assert_eq!(
        state.handle_key(key(KeyCode::Char('r')), &resorted),
        DisplayAction::Restart("beta".to_string())
    );
}

#[test]
fn disappearing_runtime_does_not_retarget_restart_to_its_old_index() {
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let telemetry = TelemetrySnapshot::default();
    let mut board = BoardSnapshot::default();
    for id in ["alpha", "beta", "gamma"] {
        board.participants.insert(
            id.to_string(),
            ParticipantStatus::new(id, ParticipantKind::Service, ParticipantState::Ready),
        );
    }
    let mut state = AppState {
        page: Page::Runtimes,
        navigation: NavigationLevel::Page,
        ..AppState::default()
    };
    let initial = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
    state.sync(&initial);
    state.handle_key(key(KeyCode::Down), &initial);
    assert_eq!(
        state.handle_key(key(KeyCode::Char('r')), &initial),
        DisplayAction::Restart("beta".to_string())
    );

    board.participants.remove("beta");
    let after_removal = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
    state.sync(&after_removal);
    assert_eq!(
        state.handle_key(key(KeyCode::Char('r')), &after_removal),
        DisplayAction::None
    );

    state.handle_key(key(KeyCode::Down), &after_removal);
    assert_eq!(
        state.handle_key(key(KeyCode::Char('r')), &after_removal),
        DisplayAction::Restart("alpha".to_string())
    );
}

#[test]
fn simulation_runtime_navigation_skips_physical_driver_rows() {
    let mut board = BoardSnapshot::default();
    board.participants.insert(
        "alpha".to_string(),
        ParticipantStatus::new("alpha", ParticipantKind::Service, ParticipantState::Ready),
    );
    board.participants.insert(
        "front_camera".to_string(),
        ParticipantStatus::new(
            "front_camera",
            ParticipantKind::Driver,
            ParticipantState::Ready,
        ),
    );
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let telemetry = TelemetrySnapshot::default();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
    let mut state = AppState {
        page: Page::Runtimes,
        navigation: NavigationLevel::Page,
        ..AppState::for_mode(SessionMode::Simulation)
    };

    state.sync(&model);
    assert_eq!(model.runtimes[state.runtime_cursor].id, "alpha");
    state.handle_key(key(KeyCode::Down), &model);
    assert_eq!(model.runtimes[state.runtime_cursor].id, "alpha");
    state.handle_key(key(KeyCode::Enter), &model);
    assert_eq!(state.runtime_detail_id.as_deref(), Some("alpha"));
}

#[test]
fn runtime_detail_arrows_scroll_topics_and_escape_resets_offset() {
    let scope = RobotScope {
        namespace: "dev".to_string(),
        robot_id: "rover".to_string(),
    };
    let mut board = BoardSnapshot::default();
    board.participants.insert(
        "alpha".to_string(),
        ParticipantStatus::new("alpha", ParticipantKind::Service, ParticipantState::Ready)
            .with_scope(scope.clone()),
    );
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let topic = |name: &str| RuntimeTopicSample {
        topic: name.to_string(),
        direction: RuntimeDirection::Publish,
        buffer_kind: RuntimeBufferKind::Outbound,
        count: 0,
        rate_hz: 0.0,
        drops: 0,
        latest_overwrites: 0,
        bounded_evictions: 0,
        capacity: 1,
        current_depth: 0,
        high_water_depth: 0,
        decode_errors: 0,
        overflowed_rows: 0,
    };
    let telemetry = TelemetrySnapshot {
        scope: Some(scope),
        runtimes: BTreeMap::from([(
            "alpha".to_string(),
            Timestamped::new(
                RuntimePerformanceSample {
                    sequence: 1,
                    participant_id: "alpha".to_string(),
                    truncated: 0,
                    window_ns: 1,
                    step: None,
                    topics: Arc::new(vec![
                        topic("one"),
                        topic("two"),
                        topic("three"),
                        topic("four"),
                        topic("five"),
                    ]),
                    overflow: None,
                },
                Instant::now(),
            ),
        )]),
        ..TelemetrySnapshot::default()
    };
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
    let mut state = AppState {
        page: Page::Runtimes,
        navigation: NavigationLevel::Page,
        ..AppState::default()
    };

    state.handle_key(key(KeyCode::Enter), &model);
    state.set_runtime_topic_viewport(5, 2);
    for _ in 0..8 {
        state.handle_key(key(KeyCode::Down), &model);
    }
    assert_eq!(state.runtime_topic_offset, 3);
    state.handle_key(key(KeyCode::Up), &model);
    assert_eq!(state.runtime_topic_offset, 2);
    state.handle_key(key(KeyCode::Esc), &model);
    assert!(state.runtime_detail_id.is_none());
    assert_eq!(state.runtime_topic_offset, 0);
}

#[test]
fn disappearing_frozen_runtime_returns_to_the_runtime_list() {
    let mut board = BoardSnapshot::default();
    for id in ["alpha", "beta"] {
        board.participants.insert(
            id.to_string(),
            ParticipantStatus::new(id, ParticipantKind::Service, ParticipantState::Ready),
        );
    }
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let telemetry = TelemetrySnapshot::default();
    let mut state = AppState {
        page: Page::Runtimes,
        navigation: NavigationLevel::Page,
        runtime_detail_id: Some("alpha".to_string()),
        ..AppState::default()
    };

    board.participants.remove("alpha");
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
    state.sync(&model);

    assert!(state.runtime_detail_id.is_none());
    assert_eq!(model.runtimes[state.runtime_cursor].id, "beta");
}

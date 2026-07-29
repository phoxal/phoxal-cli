//! Tests for this module.

use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::*;
use crate::tui::log_view::LogView;
use crate::tui::runtime_view::RuntimeView;
use phoxal_cli_core::session::ParticipantKind;
use phoxal_cli_core::session::RobotScope;
use phoxal_cli_core::session::{
    BoardSnapshot, LogSource, MotionSample, ParticipantState, ParticipantStatus, RoutedLogLine,
};
use phoxal_cli_core::session::{DeviceDiskSample, JoypadDevice};
use phoxal_cli_core::session::{
    RuntimeBufferKind, RuntimeDirection, RuntimePerformanceSample, RuntimeStepSample,
    RuntimeTopicSample,
};

fn title() -> TitleInfo {
    TitleInfo {
        robot: "rover".to_string(),
        namespace: "dev".to_string(),
        train: "0.36.0".to_string(),
        manifest: "./robot.yaml".to_string(),
        mode: SessionMode::Run,
        bus_endpoint: "tcp/localhost:7447".to_string(),
        simulation_profile: None,
        simulation_world: None,
        started_at: UNIX_EPOCH,
        started_instant: Instant::now(),
    }
}

#[test]
fn startup_phase_text_is_sanitized_and_bounded() {
    let phase = PhaseRow {
        id: phoxal_cli_core::session::event::PhaseId::new("prepare"),
        label: format!("{}\u{1b}[2J", "phase".repeat(20)),
        progress: Some(crate::tui::startup::PhaseProgressInfo {
            completed: 1,
            total: 2,
            detail: Some(format!("{}\u{1b}]0;owned\u{7}", "detail".repeat(20))),
        }),
        outcome: Some((
            PhaseOutcome::Failed {
                error: format!("{}\u{1b}[7;39H", "failure".repeat(20)),
            },
            Duration::from_secs(1),
        )),
    };
    let rendered = startup_phase_lines(&phase)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!rendered.contains('\u{1b}'));
    assert!(rendered.lines().all(|line| line.chars().count() < 100));
}

fn render_page(page: Page, telemetry: &TelemetrySnapshot) -> String {
    render_page_at(page, telemetry, 100, 28)
}

fn render_page_at(page: Page, telemetry: &TelemetrySnapshot, width: u16, height: u16) -> String {
    let board = BoardSnapshot::default();
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let model = SessionViewModel::new(&board, &logs, &runtime, telemetry, Instant::now());
    let mut state = AppState::default();
    state.page = page;
    render_model(&title(), &state, &model, width, height)
}

fn render_model(
    title: &TitleInfo,
    state: &AppState,
    model: &SessionViewModel<'_>,
    width: u16,
    height: u16,
) -> String {
    render_model_with_state(title, state, model, width, height).0
}

fn render_model_with_state(
    title: &TitleInfo,
    state: &AppState,
    model: &SessionViewModel<'_>,
    width: u16,
    height: u16,
) -> (String, AppState) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut render_state = state.clone();
    terminal
        .draw(|frame| {
            draw(
                frame,
                Theme::new(ColorCapability::None),
                title,
                &mut render_state,
                model,
            );
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let rendered = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    (rendered, render_state)
}

fn render_startup_at(width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let startup = StartupState::new();
    let state = AppState::default();
    let telemetry = TelemetrySnapshot::default();
    let title = title();
    terminal
        .draw(|frame| {
            draw_startup(
                frame,
                Theme::new(ColorCapability::None),
                &StartupView {
                    title: &title,
                    startup: &startup,
                    state: &state,
                    telemetry: &telemetry,
                    now: Instant::now(),
                },
            );
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn startup_surface_renders_normal_and_too_small_layouts() {
    let normal = render_startup_at(100, 28);
    assert!(normal.contains("Starting session"), "{normal}");
    assert!(normal.contains("preparing"), "{normal}");
    let small = render_startup_at(43, 17);
    assert!(small.contains("Terminal too small"), "{small}");
}

#[test]
fn every_fixed_page_renders_with_empty_data() {
    for page in Page::ALL {
        let rendered = render_page(page, &TelemetrySnapshot::default());
        assert!(rendered.contains(page.label()), "{page:?}: {rendered}");
        assert!(rendered.contains("Overview"));
        assert!(rendered.contains("Runtimes"));
        assert!(rendered.contains("Logs"));
        assert!(rendered.contains("Bus"));
        assert!(rendered.contains("Input"));
    }
}

#[test]
fn responsive_header_and_tabs_cover_compact_expanded_and_too_small_sizes() {
    let now = Instant::now();
    let telemetry = TelemetrySnapshot {
        device: Some(Timestamped {
            received_at: now,
            value: DeviceSample {
                cpu_pct: Some(10.0),
                ram_used_bytes: Some(2),
                ram_total_bytes: Some(4),
                load_1m: Some(0.1),
                load_5m: Some(0.2),
                load_15m: Some(0.3),
                disks: Some(
                    vec![DeviceDiskSample {
                        mount_point: "/".to_string(),
                        used_bytes: 10,
                        total_bytes: 100,
                        ..DeviceDiskSample::default()
                    }]
                    .into(),
                ),
                ..DeviceSample::default()
            },
        }),
        clock: Some(Timestamped {
            received_at: now,
            value: ClockSample {
                now_ns: 5_000_000_000,
                step: 42,
            },
        }),
        ..TelemetrySnapshot::default()
    };
    let board = BoardSnapshot::default();
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, now);
    let mut simulation_title = title();
    simulation_title.mode = SessionMode::Simulation;

    let compact = render_model(&simulation_title, &AppState::default(), &model, 44, 18);
    assert!(compact.contains("cpu 10%"), "{compact}");
    assert!(compact.contains("step 42"), "{compact}");
    assert!(compact.contains("Input"), "{compact}");

    let expanded = render_model(&simulation_title, &AppState::default(), &model, 80, 24);
    assert!(expanded.contains("Device"), "{expanded}");
    assert!(expanded.contains("Simulation"), "{expanded}");
    assert!(expanded.contains("DISK (root)"), "{expanded}");

    let too_small = render_model(&simulation_title, &AppState::default(), &model, 44, 12);
    assert!(too_small.contains("Resize to at least 44 x 18"));
}

#[test]
fn help_renders_product_and_issue_links() {
    let telemetry = TelemetrySnapshot::default();
    let board = BoardSnapshot::default();
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
    let mut state = AppState::default();
    state.show_help = true;
    let rendered = render_model(&title(), &state, &model, 80, 24);
    assert!(rendered.contains("https://phoxal.com"));
    assert!(rendered.contains("github.com/phoxal/phoxal-cli/issues"));

    let compact = render_model(&title(), &state, &model, 44, 18);
    let without_whitespace = compact
        .chars()
        .filter(|character| character.is_ascii() && !character.is_whitespace())
        .collect::<String>();
    assert!(
        without_whitespace.contains("github.com/phoxal/phoxal-cli/issues"),
        "{compact}"
    );

    state.show_help = false;
    state.show_info = true;
    let compact = render_model(&title(), &state, &model, 44, 18);
    assert!(compact.contains("start time"), "{compact}");
}

#[test]
fn webots_session_information_contains_only_profile_world_and_process_state() {
    let telemetry = TelemetrySnapshot::default();
    let board = BoardSnapshot::default();
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
    let mut state = AppState::default();
    state.show_info = true;
    let mut simulation_title = title();
    simulation_title.mode = SessionMode::Simulation;
    simulation_title.simulation_profile = Some("webots".to_string());
    simulation_title.simulation_world = Some("worlds/default.wbt".to_string());

    let rendered = render_model(&simulation_title, &state, &model, 80, 24);
    assert!(rendered.contains("simulation       webots"), "{rendered}");
    assert!(
        rendered.contains("world            worlds/default.wbt"),
        "{rendered}"
    );
    assert!(
        rendered.contains("Webots process   not started"),
        "{rendered}"
    );
}

#[test]
fn narrow_pages_keep_selected_controls_and_global_help_visible() {
    let telemetry = TelemetrySnapshot::default();
    let board = BoardSnapshot::default();
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());

    let mut logs_state = AppState::default();
    logs_state.page = Page::Logs;
    logs_state.navigation = NavigationLevel::Page;
    logs_state.log_filter_cursor = 3;
    logs_state.log_text_filter = "needle".to_string();
    let rendered = render_model(&title(), &logs_state, &model, 44, 18);
    assert!(rendered.contains("4/5 Contains: needle"), "{rendered}");
    assert!(rendered.contains("? help · q quit"), "{rendered}");

    logs_state.page = Page::Bus;
    logs_state.bus_control_cursor = 2;
    logs_state.bus_show_internal = true;
    let rendered = render_model(&title(), &logs_state, &model, 44, 18);
    assert!(
        rendered.contains("3/3 Internal topics: Shown"),
        "{rendered}"
    );
    assert!(rendered.contains("? help · q quit"), "{rendered}");
}

#[test]
fn tools_source_filter_renders_tool_logs() {
    let board = BoardSnapshot::default();
    let mut logs = LogView::new();
    logs.record(RoutedLogLine {
        participant: ROBOT_TOOL_JOYPAD.to_string(),
        source: LogSource::Bus,
        severity: LogSeverity::Info,
        text: "joypad ready".to_string(),
        event_time: std::time::SystemTime::UNIX_EPOCH,
        scope: None,
    });
    logs.record(RoutedLogLine {
        participant: "phoxal-cli/Cli".to_string(),
        source: LogSource::Raw,
        severity: LogSeverity::Info,
        text: "cli diagnostic".to_string(),
        event_time: std::time::SystemTime::UNIX_EPOCH,
        scope: None,
    });
    let runtime = RuntimeView::new();
    let telemetry = TelemetrySnapshot::default();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
    let mut state = AppState::default();
    state.page = Page::Logs;
    state.log_source_filter = LogSourceFilter::Tools;
    let rendered = render_model(&title(), &state, &model, 100, 28);
    assert!(rendered.contains("Source: Tools"));
    assert!(rendered.contains("joypad ready"));
    assert!(rendered.contains("cli diagnostic"));
}

#[test]
fn runtime_page_keeps_the_three_group_boxes_when_empty() {
    let rendered = render_page(Page::Runtimes, &TelemetrySnapshot::default());
    for label in ["User services", "Framework services", "Drivers"] {
        assert!(rendered.contains(label), "missing {label}: {rendered}");
    }
    assert!(rendered.contains("No user runtimes"));
}

fn runtime_scope() -> RobotScope {
    RobotScope {
        namespace: "dev".to_string(),
        robot_id: "rover".to_string(),
    }
}

fn performance_sample(
    participant_id: &str,
    received_at: Instant,
) -> Timestamped<RuntimePerformanceSample> {
    Timestamped::new(
        RuntimePerformanceSample {
            sequence: 1,
            participant_id: participant_id.to_string(),
            truncated: 2,
            window_ns: 1_000_000_000,
            step: Some(RuntimeStepSample {
                target_period_ns: 20_000_000,
                completed: 50,
                errors: 1,
                mean_duration_ns: 8_000_000,
                max_duration_ns: 10_000_000,
                mean_lateness_ns: 1_000_000,
                max_lateness_ns: 3_000_000,
                missed_ticks: 2,
                overruns: 3,
            }),
            topics: vec![RuntimeTopicSample {
                topic: "v1/drive/target".to_string(),
                direction: RuntimeDirection::Subscribe,
                buffer_kind: RuntimeBufferKind::Subscriber,
                count: 10,
                rate_hz: 10.0,
                drops: 1,
                latest_overwrites: 0,
                bounded_evictions: 0,
                capacity: 8,
                current_depth: 2,
                high_water_depth: 4,
                decode_errors: 0,
                overflowed_rows: 0,
            }]
            .into(),
            overflow: Some(RuntimeTopicSample {
                topic: "Other/unobserved topics".to_string(),
                direction: RuntimeDirection::Mixed,
                buffer_kind: RuntimeBufferKind::Mixed,
                count: 0,
                rate_hz: 0.0,
                drops: 0,
                latest_overwrites: 0,
                bounded_evictions: 0,
                capacity: 0,
                current_depth: 0,
                high_water_depth: 0,
                decode_errors: 0,
                overflowed_rows: 3,
            }),
        },
        received_at,
    )
}

#[test]
fn runtime_rows_distinguish_fresh_stalled_and_missing_portable_progress() {
    let now = Instant::now();
    let mut board = BoardSnapshot::default();
    let mut fresh =
        ParticipantStatus::new("fresh", ParticipantKind::Service, ParticipantState::Ready)
            .with_scope(runtime_scope());
    fresh.present = Some(true);
    let mut stalled =
        ParticipantStatus::new("stalled", ParticipantKind::Service, ParticipantState::Ready)
            .with_scope(runtime_scope());
    stalled.present = Some(true);
    let mut missing = ParticipantStatus::new(
        "missing",
        ParticipantKind::Service,
        ParticipantState::Degraded,
    )
    .with_scope(runtime_scope());
    missing.present = Some(false);
    let unknown_presence = ParticipantStatus::new(
        "unknown-presence",
        ParticipantKind::Service,
        ParticipantState::Ready,
    )
    .with_scope(runtime_scope());
    for status in [fresh, stalled, missing, unknown_presence] {
        board.participants.insert(status.id.clone(), status);
    }
    let telemetry = TelemetrySnapshot {
        scope: Some(runtime_scope()),
        runtimes: BTreeMap::from([
            ("fresh".to_string(), performance_sample("fresh", now)),
            (
                "stalled".to_string(),
                performance_sample("stalled", now - Duration::from_secs(4)),
            ),
            ("missing".to_string(), performance_sample("missing", now)),
            (
                "unknown-presence".to_string(),
                performance_sample("unknown-presence", now - Duration::from_secs(4)),
            ),
        ]),
        ..TelemetrySnapshot::default()
    };
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, now);
    let wide = runtime_columns(100);

    assert!(runtime_row(board.participants.get("fresh").unwrap(), &model, wide).contains("10.0/s"));
    assert!(
        runtime_row(board.participants.get("stalled").unwrap(), &model, wide).contains("stalled")
    );
    assert!(
        runtime_row(board.participants.get("missing").unwrap(), &model, wide).contains("missing")
    );
    assert!(
        runtime_row(
            board.participants.get("unknown-presence").unwrap(),
            &model,
            wide,
        )
        .contains("stalled")
    );
}

#[test]
fn runtime_row_never_uses_same_id_telemetry_from_another_robot() {
    let now = Instant::now();
    let other_scope = RobotScope {
        namespace: "dev".to_string(),
        robot_id: "other".to_string(),
    };
    let status = ParticipantStatus::new("drive", ParticipantKind::Service, ParticipantState::Ready)
        .with_scope(other_scope);
    let mut board = BoardSnapshot::default();
    board.participants.insert(status.id.clone(), status);
    let telemetry = TelemetrySnapshot {
        scope: Some(runtime_scope()),
        runtimes: BTreeMap::from([("drive".to_string(), performance_sample("drive", now))]),
        ..TelemetrySnapshot::default()
    };
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, now);

    let row = runtime_row(
        board.participants.get("drive").unwrap(),
        &model,
        runtime_columns(100),
    );
    assert!(row.contains("not shown"), "{row}");
    assert!(!row.contains("10.0/s"), "{row}");

    let mut state = AppState::default();
    state.page = Page::Runtimes;
    state.runtime_detail_id = Some("drive".to_string());
    let detail = render_model(&title(), &state, &model, 100, 30);
    assert!(detail.contains("telemetry not shown"), "{detail}");
    assert!(!detail.contains("10.0/s"), "{detail}");
    assert!(!detail.contains("v1/drive/target"), "{detail}");
}

#[test]
fn runtime_detail_renders_portable_summary_and_topic_pressure() {
    let now = Instant::now();
    let mut board = BoardSnapshot::default();
    let mut status =
        ParticipantStatus::new("drive", ParticipantKind::Service, ParticipantState::Ready)
            .with_scope(runtime_scope());
    status.present = Some(true);
    board.participants.insert(status.id.clone(), status);
    let telemetry = TelemetrySnapshot {
        scope: Some(runtime_scope()),
        runtimes: BTreeMap::from([("drive".to_string(), performance_sample("drive", now))]),
        ..TelemetrySnapshot::default()
    };
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, now);
    let mut state = AppState::default();
    state.page = Page::Runtimes;
    state.navigation = NavigationLevel::Page;
    state.runtime_detail_id = Some("drive".to_string());

    let rendered = render_model(&title(), &state, &model, 120, 36);
    assert!(rendered.contains("Portable performance"), "{rendered}");
    assert!(rendered.contains("budget"), "{rendered}");
    assert!(rendered.contains("v1/drive/target"), "{rendered}");
    assert!(rendered.contains("DEPTH"), "{rendered}");
    assert!(rendered.contains("peak budget"), "{rendered}");
    assert!(rendered.contains("duration mean 8.0ms"), "{rendered}");
    assert!(rendered.contains("lateness mean 1.0ms"), "{rendered}");
    assert!(rendered.contains("missed 2"), "{rendered}");
    assert!(rendered.contains("trunc 2"), "{rendered}");
    assert!(rendered.contains("3 rows aggregated"), "{rendered}");

    let compact = render_model(&title(), &state, &model, 80, 30);
    assert!(compact.contains("COUNT"), "{compact}");
    assert!(compact.contains("10"), "{compact}");

    let minimum = render_model(&title(), &state, &model, 44, 18);
    assert!(minimum.contains("performance"), "{minimum}");
    assert!(minimum.contains("Topics"), "{minimum}");
    assert!(minimum.contains("v1/dri"), "{minimum}");
}

#[test]
fn runtime_detail_compacts_extreme_counters_without_clipping_them() {
    let now = Instant::now();
    let mut board = BoardSnapshot::default();
    let mut status =
        ParticipantStatus::new("drive", ParticipantKind::Service, ParticipantState::Ready)
            .with_scope(runtime_scope());
    status.present = Some(true);
    board.participants.insert(status.id.clone(), status);
    let mut sample = performance_sample("drive", now);
    let value = &mut sample.value;
    let step = value.step.as_mut().unwrap();
    step.errors = u64::MAX;
    step.missed_ticks = u64::MAX;
    step.overruns = u64::MAX;
    value.topics = vec![RuntimeTopicSample {
        count: u64::MAX,
        drops: u64::MAX,
        ..value.topics[0].clone()
    }]
    .into();
    let telemetry = TelemetrySnapshot {
        scope: Some(runtime_scope()),
        runtimes: BTreeMap::from([("drive".to_string(), sample)]),
        ..TelemetrySnapshot::default()
    };
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, now);
    let mut state = AppState::default();
    state.page = Page::Runtimes;
    state.navigation = NavigationLevel::Page;
    state.runtime_detail_id = Some("drive".to_string());

    let rendered = render_model(&title(), &state, &model, 120, 30);
    assert!(rendered.matches("18.4E").count() >= 4, "{rendered}");
    assert!(rendered.contains("COUNT"), "{rendered}");
}

#[test]
fn runtime_topic_renderer_clamps_to_the_viewport_before_up_moves() {
    let now = Instant::now();
    let mut board = BoardSnapshot::default();
    let mut status =
        ParticipantStatus::new("drive", ParticipantKind::Service, ParticipantState::Ready)
            .with_scope(runtime_scope());
    status.present = Some(true);
    board.participants.insert(status.id.clone(), status);
    let mut sample = performance_sample("drive", now);
    let template = sample.value.topics[0].clone();
    sample.value.topics = (0..40)
        .map(|index| RuntimeTopicSample {
            topic: format!("topic-{index}"),
            ..template.clone()
        })
        .collect::<Vec<_>>()
        .into();
    sample.value.overflow = None;
    let telemetry = TelemetrySnapshot {
        scope: Some(runtime_scope()),
        runtimes: BTreeMap::from([("drive".to_string(), sample)]),
        ..TelemetrySnapshot::default()
    };
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, now);
    let mut state = AppState::default();
    state.page = Page::Runtimes;
    state.navigation = NavigationLevel::Page;
    state.runtime_detail_id = Some("drive".to_string());
    state.runtime_topic_offset = usize::MAX;

    // 40 topics against a detail panel that shows 5 rows at 80x30, so the
    // clamp lands at 35 and there is plenty of range left for `Up` to move
    // through. The exact figure encodes the panel's capacity: if a layout
    // change moves it, this is the test that should say so.
    let (_, mut rendered_state) = render_model_with_state(&title(), &state, &model, 80, 30);
    assert_eq!(rendered_state.runtime_topic_offset, 35);
    rendered_state.handle_key(
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Up,
            crossterm::event::KeyModifiers::NONE,
        ),
        &model,
    );
    assert_eq!(rendered_state.runtime_topic_offset, 34);
}

#[test]
fn missing_runtime_detail_hides_cached_metrics_and_topics() {
    let now = Instant::now();
    let mut status = ParticipantStatus::new(
        "drive",
        ParticipantKind::Service,
        ParticipantState::Degraded,
    )
    .with_scope(runtime_scope());
    status.present = Some(false);
    let mut board = BoardSnapshot::default();
    board.participants.insert(status.id.clone(), status);
    let telemetry = TelemetrySnapshot {
        scope: Some(runtime_scope()),
        runtimes: BTreeMap::from([("drive".to_string(), performance_sample("drive", now))]),
        ..TelemetrySnapshot::default()
    };
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, now);
    let mut state = AppState::default();
    state.page = Page::Runtimes;
    state.runtime_detail_id = Some("drive".to_string());

    let rendered = render_model(&title(), &state, &model, 120, 36);
    assert!(rendered.contains("telemetry missing"), "{rendered}");
    assert!(!rendered.contains("10.0/s"), "{rendered}");
    assert!(!rendered.contains("v1/drive/target"), "{rendered}");
}

#[test]
fn runtime_group_layout_preserves_every_box_and_distributes_extra_rows() {
    assert_eq!(runtime_section_heights([0, 0, 0], 12), [4, 4, 4]);
    assert_eq!(runtime_section_heights([0, 0, 0], 15), [5, 5, 5]);
    let distributed = runtime_section_heights([0, 12, 6], 20);
    assert_eq!(distributed.iter().sum::<u16>(), 20);
    assert!(distributed.into_iter().all(|height| height >= 4));
    assert!(distributed[1] > distributed[0]);
    assert!(distributed[2] > distributed[0]);
}

#[test]
fn wide_runtime_columns_give_surplus_width_to_identity() {
    let RuntimeColumns::Wide { id: base, .. } = runtime_columns(92) else {
        panic!("92 columns should use the wide layout");
    };
    let RuntimeColumns::Wide { id: expanded, .. } = runtime_columns(120) else {
        panic!("120 columns should use the wide layout");
    };
    assert_eq!(expanded - base, 28);
}

#[test]
fn clipped_runtime_groups_report_how_many_rows_are_shown() {
    let mut board = BoardSnapshot::default();
    for id in ["alpha", "beta", "gamma", "delta"] {
        board.participants.insert(
            id.to_string(),
            ParticipantStatus::new(id, ParticipantKind::Service, ParticipantState::Ready),
        );
    }
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let telemetry = TelemetrySnapshot::default();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
    let mut state = AppState::default();
    state.page = Page::Runtimes;
    state.navigation = NavigationLevel::Page;

    let rendered = render_model(&title(), &state, &model, 44, 18);
    assert!(
        rendered.contains("Framework services · 1-1 of 4"),
        "{rendered}"
    );
}

#[test]
fn runtime_header_stays_aligned_when_the_row_is_selected() {
    let mut board = BoardSnapshot::default();
    board.participants.insert(
        "alpha".to_string(),
        ParticipantStatus::new("alpha", ParticipantKind::Service, ParticipantState::Ready),
    );
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let telemetry = TelemetrySnapshot::default();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
    let mut state = AppState::default();
    state.page = Page::Runtimes;
    state.navigation = NavigationLevel::Page;
    let rendered = render_model(&title(), &state, &model, 100, 28);
    let header = rendered.lines().find(|line| line.contains("ID")).unwrap();
    let row = rendered
        .lines()
        .find(|line| line.contains("alpha"))
        .unwrap();
    assert_eq!(
        header.chars().position(|character| character == 'I'),
        row.chars().position(|character| character == 'a'),
        "{rendered}"
    );
}

#[test]
fn simulation_runtime_page_replaces_driver_rows_with_placeholder() {
    let mut board = BoardSnapshot::default();
    board.participants.insert(
        "front_camera".to_string(),
        ParticipantStatus::new(
            "front_camera",
            ParticipantKind::Driver,
            ParticipantState::Degraded,
        ),
    );
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let telemetry = TelemetrySnapshot::default();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
    let mut state = AppState::for_mode(SessionMode::Simulation);
    state.page = Page::Runtimes;
    let mut simulation_title = title();
    simulation_title.mode = SessionMode::Simulation;
    let rendered = render_model(&simulation_title, &state, &model, 100, 28);

    assert!(rendered.contains("Not loaded in simulation"));
    assert!(!rendered.contains("front_camera"));

    state.page = Page::Overview;
    let rendered = render_model(&simulation_title, &state, &model, 100, 28);
    assert!(rendered.contains("degraded 0"), "{rendered}");
    assert!(!rendered.contains("front_camera"), "{rendered}");
}

#[test]
fn bus_page_renders_total_history_and_per_producer_rate() {
    let now = Instant::now();
    let telemetry = TelemetrySnapshot {
        router: Some(Timestamped {
            received_at: now,
            value: phoxal_cli_core::session::RouterMetricsSample {
                topics: vec![TopicMetric {
                    topic: "dev/robots/rover/v1/motion/state".to_string(),
                    from_participant: "motion".to_string(),
                    ingress_rate_hz: 12.0,
                    count: 99,
                    aggregate_overflow: false,
                }]
                .into(),
                topics_truncated: 0,
                throughput_msg_s: 12.0,
                window_ns: 1_000_000_000,
            },
        }),
        router_throughput_history: vec![Timestamped {
            received_at: now,
            value: 12.0,
        }],
        ..TelemetrySnapshot::default()
    };
    let rendered = render_page(Page::Bus, &telemetry);
    assert!(rendered.contains("12.0 messages/s"));
    assert!(rendered.contains("All producers"));
    assert!(rendered.contains("motion"));

    let compact = render_page_at(Page::Bus, &telemetry, 44, 18);
    assert!(compact.contains("12.0 Hz"), "{compact}");
    assert!(compact.contains("99"), "{compact}");
}

#[test]
fn bus_producer_summary_aggregates_all_topics_for_one_runtime() {
    let now = Instant::now();
    let telemetry = TelemetrySnapshot {
        router: Some(Timestamped {
            received_at: now,
            value: phoxal_cli_core::session::RouterMetricsSample {
                topics: vec![
                    TopicMetric {
                        topic: "v1/motion/state".to_string(),
                        from_participant: "motion".to_string(),
                        ingress_rate_hz: 2.0,
                        count: 2,
                        aggregate_overflow: false,
                    },
                    TopicMetric {
                        topic: "v1/motion/target".to_string(),
                        from_participant: "motion".to_string(),
                        ingress_rate_hz: 3.0,
                        count: 3,
                        aggregate_overflow: false,
                    },
                ]
                .into(),
                throughput_msg_s: 5.0,
                ..phoxal_cli_core::session::RouterMetricsSample::default()
            },
        }),
        ..TelemetrySnapshot::default()
    };

    let rendered = render_page_at(Page::Bus, &telemetry, 100, 28);
    let producer_row = rendered
        .lines()
        .find(|line| line.contains("motion") && line.contains("5.0"))
        .expect("aggregated producer row");
    assert!(producer_row.contains('5'), "{producer_row}");
}

#[test]
fn bus_producer_summary_discloses_capped_traffic_without_inventing_a_producer() {
    let now = Instant::now();
    let telemetry = TelemetrySnapshot {
        router: Some(Timestamped {
            received_at: now,
            value: phoxal_cli_core::session::RouterMetricsSample {
                topics: vec![TopicMetric {
                    topic: "Other/unobserved traffic".to_string(),
                    from_participant: "multiple".to_string(),
                    ingress_rate_hz: 4.0,
                    count: 20,
                    aggregate_overflow: true,
                }]
                .into(),
                topics_truncated: 0,
                throughput_msg_s: 4.0,
                window_ns: 1_000_000_000,
            },
        }),
        ..TelemetrySnapshot::default()
    };
    let rendered = render_page_at(Page::Bus, &telemetry, 100, 28);
    assert!(
        rendered.contains("Overflow excluded; total still includes it"),
        "{rendered}"
    );
    assert!(rendered.contains("Other/unobserved traffic"), "{rendered}");
    assert!(rendered.contains("1/1 visible"), "{rendered}");
    assert!(rendered.contains("aggregate"), "{rendered}");
    assert!(!rendered.contains("multiple"), "{rendered}");
}

#[test]
fn input_page_keeps_tool_errors_in_logs_only() {
    let now = Instant::now();
    let telemetry = TelemetrySnapshot {
        joypad: Some(Timestamped {
            received_at: now,
            value: JoypadDevicesSample {
                last_error: Some("selection failed".to_string()),
                ..JoypadDevicesSample::default()
            },
        }),
        ..TelemetrySnapshot::default()
    };
    let rendered = render_page(Page::Input, &telemetry);
    assert!(!rendered.contains("selection failed"));
    assert!(!rendered.contains("Last error"));

    let unavailable = TelemetrySnapshot {
        joypad: Some(Timestamped {
            received_at: now,
            value: JoypadDevicesSample {
                unavailable_reason: Some("gamepad backend unavailable".to_string()),
                ..JoypadDevicesSample::default()
            },
        }),
        ..TelemetrySnapshot::default()
    };
    let rendered = render_page(Page::Input, &unavailable);
    assert!(
        rendered.contains("Input unavailable · gamepad backend unavailable"),
        "{rendered}"
    );

    let unavailable_with_device = TelemetrySnapshot {
        joypad: Some(Timestamped {
            received_at: now,
            value: JoypadDevicesSample {
                available: vec![JoypadDevice {
                    id: "pad".to_string(),
                    name: "Pad".to_string(),
                    status: JoypadDeviceStatus::Ready,
                }]
                .into(),
                unavailable_reason: Some(
                    "manual input requires differential kinematics".to_string(),
                ),
                ..JoypadDevicesSample::default()
            },
        }),
        ..TelemetrySnapshot::default()
    };
    let rendered = render_page(Page::Input, &unavailable_with_device);
    assert!(
        rendered.contains("Devices · Input unavailable"),
        "{rendered}"
    );
    assert!(rendered.contains("manual input requires"), "{rendered}");
}

#[test]
fn motion_panel_stays_compact_when_input_state_is_stale() {
    let now = Instant::now();
    let old = now - DEFAULT_FRESHNESS_TTL - Duration::from_secs(1);
    let telemetry = TelemetrySnapshot {
        joypad: Some(Timestamped {
            received_at: old,
            value: JoypadDevicesSample {
                available: vec![JoypadDevice {
                    id: "stale-pad".to_string(),
                    name: "Stale Pad".to_string(),
                    status: JoypadDeviceStatus::Ready,
                }]
                .into(),
                selected: Some("stale-pad".to_string()),
                enabled: true,
                ..JoypadDevicesSample::default()
            },
        }),
        motion: Some(Timestamped {
            received_at: old,
            value: MotionSample {
                linear_x_mps: 0.0,
                angular_z_radps: 0.0,
            },
        }),
        ..TelemetrySnapshot::default()
    };
    let mut board = BoardSnapshot::default();
    board.participants.insert(
        ROBOT_TOOL_JOYPAD.to_string(),
        ParticipantStatus::new(
            ROBOT_TOOL_JOYPAD,
            ParticipantKind::Tool,
            ParticipantState::Ready,
        ),
    );
    let mut runtime = RuntimeView::new();
    runtime.observe_board(&board);
    let logs = LogView::new();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, now);
    let mut state = AppState::default();
    state.page = Page::Input;
    let rendered = render_model(&title(), &state, &model, 100, 28);
    assert!(rendered.contains("Motion"));
    assert!(rendered.contains("Command"));
    assert!(rendered.contains("Motion update"));
    assert!(
        rendered
            .lines()
            .any(|line| line.contains("angular 0.000 rad/s")),
        "{rendered}"
    );
    assert!(rendered.contains("stale"));
    for removed in [
        "Manual control",
        "Selected",
        "Connection",
        "Command source",
        "Joypad heartbeat",
        "Device state",
        "Robot model",
    ] {
        assert!(
            !rendered.contains(removed),
            "unexpected {removed}: {rendered}"
        );
    }
}

#[test]
fn simulation_pause_does_not_age_joypad_from_logical_clock() {
    let now = Instant::now();
    let telemetry = TelemetrySnapshot {
        clock: Some(Timestamped {
            received_at: now,
            value: ClockSample { now_ns: 0, step: 7 },
        }),
        joypad: Some(Timestamped {
            received_at: now,
            value: JoypadDevicesSample::default(),
        }),
        ..TelemetrySnapshot::default()
    };
    let mut board = BoardSnapshot::default();
    board.participants.insert(
        ROBOT_TOOL_JOYPAD.to_string(),
        ParticipantStatus::new(
            ROBOT_TOOL_JOYPAD,
            ParticipantKind::Tool,
            ParticipantState::Ready,
        ),
    );
    let mut runtime = RuntimeView::new();
    runtime.observe_board(&board);
    let logs = LogView::new();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, now);
    let mut state = AppState::for_mode(SessionMode::Simulation);
    state.page = Page::Input;
    let mut sim_title = title();
    sim_title.mode = SessionMode::Simulation;

    let rendered = render_model(&sim_title, &state, &model, 100, 28);
    assert!(rendered.contains("step    7"), "{rendered}");
    assert!(rendered.contains("Motion"), "{rendered}");
}

#[test]
fn stale_simulation_clock_is_presented_as_paused() {
    let now = Instant::now();
    let telemetry = TelemetrySnapshot {
        clock: Some(Timestamped {
            received_at: now - DEFAULT_FRESHNESS_TTL - Duration::from_millis(1),
            value: ClockSample {
                now_ns: 2_000_000_000,
                step: 9,
            },
        }),
        ..TelemetrySnapshot::default()
    };
    let board = BoardSnapshot::default();
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, now);
    let mut simulation_title = title();
    simulation_title.mode = SessionMode::Simulation;

    let rendered = render_model(&simulation_title, &AppState::default(), &model, 100, 28);
    assert!(rendered.contains("state   paused"), "{rendered}");
    assert!(rendered.contains("step    9"), "{rendered}");
}

#[test]
fn header_renders_root_disk_and_staleness() {
    let old = Instant::now() - DEFAULT_FRESHNESS_TTL - Duration::from_secs(1);
    let device = Timestamped {
        received_at: old,
        value: DeviceSample {
            cpu_pct: Some(10.0),
            ram_used_bytes: Some(2),
            ram_total_bytes: Some(4),
            load_1m: Some(0.1),
            load_5m: Some(0.2),
            load_15m: Some(0.3),
            uptime_s: Some(65),
            disks: Some(
                vec![DeviceDiskSample {
                    mount_point: "/".to_string(),
                    file_system: "apfs".to_string(),
                    used_bytes: 10,
                    total_bytes: 100,
                }]
                .into(),
            ),
            ..DeviceSample::default()
        },
    };
    let lines = header_device_lines(Some(&device), Instant::now(), 40)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(lines.contains("DISK (root)"));
    assert!(lines.contains("stale"));

    let telemetry = TelemetrySnapshot {
        device: Some(device),
        ..TelemetrySnapshot::default()
    };
    let rendered = render_page_at(Page::Overview, &telemetry, 80, 24);
    assert!(rendered.contains("0.1/0.2/0.3"), "{rendered}");

    let unavailable = Timestamped {
        received_at: Instant::now(),
        value: DeviceSample {
            ..DeviceSample::default()
        },
    };
    let unavailable_lines = header_device_lines(Some(&unavailable), Instant::now(), 40)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(unavailable_lines.contains("CPU         n/a"));
    assert!(unavailable_lines.contains("RAM         n/a"));
}

#[test]
fn bus_sort_is_deterministic_for_rate_topic_and_producer() {
    let a = TopicMetric {
        topic: "z/topic".to_string(),
        from_participant: "alpha".to_string(),
        ingress_rate_hz: 1.0,
        count: 1,
        aggregate_overflow: false,
    };
    let b = TopicMetric {
        topic: "a/topic".to_string(),
        from_participant: "zeta".to_string(),
        ingress_rate_hz: 5.0,
        count: 2,
        aggregate_overflow: false,
    };
    let mut topics = vec![&a, &b];
    sort_topics(&mut topics, BusSort::Rate);
    assert_eq!(topics[0].topic, "a/topic");
    sort_topics(&mut topics, BusSort::Topic);
    assert_eq!(topics[0].topic, "a/topic");
    sort_topics(&mut topics, BusSort::Producer);
    assert_eq!(topics[0].from_participant, "alpha");
}

#[test]
fn overscroll_keeps_a_full_oldest_window_visible() {
    assert_eq!(bounded_window_start(usize::MAX, 10, 4), 6);
    assert_eq!(bounded_window_start(usize::MAX, 3, 4), 0);
}

#[test]
fn remote_cell_text_is_sanitized_and_ellipsized() {
    assert_eq!(sanitize_and_ellipsize("alpha\u{1b}[2Jbeta", 8), "alphabe…");
    assert_eq!(
        sanitize_and_ellipsize("alpha\u{202e}beta", 10),
        "alpha beta"
    );
    assert_eq!(ellipsize("controller", 1), "…");
    assert_eq!(ellipsize("pad", 8), "pad");
    assert_eq!(ellipsize("控制器", 5), "控制…");
    assert_eq!(
        UnicodeWidthStr::width(sanitize_and_fit_cell("控制", 6).as_str()),
        6
    );
}

#[test]
fn bus_history_title_describes_the_observed_window() {
    let now = Instant::now();
    assert_eq!(history_span(&[]), "waiting");
    assert_eq!(
        history_span(&[Timestamped {
            received_at: now,
            value: 1.0,
        }],),
        "1 sample"
    );
    assert_eq!(
        history_span(&[
            Timestamped {
                received_at: now - Duration::from_secs(8),
                value: 1.0,
            },
            Timestamped {
                received_at: now,
                value: 2.0,
            },
        ],),
        "last 8.0s"
    );
}

#[test]
fn bus_history_uses_the_newest_samples_that_fit_the_graph() {
    let history = [1, 2, 3, 4, 5];
    assert_eq!(history_tail(&history, 3), [3, 4, 5]);
    assert_eq!(history_tail(&history, 10), history);
}

#[test]
fn bus_graph_title_matches_the_samples_that_fit_the_panel() {
    let now = Instant::now();
    let telemetry = TelemetrySnapshot {
        router: Some(Timestamped {
            received_at: now,
            value: phoxal_cli_core::session::RouterMetricsSample {
                throughput_msg_s: 59.0,
                ..phoxal_cli_core::session::RouterMetricsSample::default()
            },
        }),
        router_throughput_history: (0..60)
            .map(|seconds| Timestamped {
                received_at: now - Duration::from_secs(59 - seconds),
                value: seconds as f32,
            })
            .collect(),
        ..TelemetrySnapshot::default()
    };
    let rendered = render_page_at(Page::Bus, &telemetry, 120, 28);
    assert!(rendered.contains("last 51.0s"), "{rendered}");
}

#[test]
fn persistent_header_sanitizes_identity_fields() {
    let telemetry = TelemetrySnapshot::default();
    let board = BoardSnapshot::default();
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
    let mut unsafe_title = title();
    unsafe_title.robot = "rover\u{202e}spoof".to_string();
    unsafe_title.namespace = "dev\u{1b}[2J".to_string();
    let rendered = render_model(&unsafe_title, &AppState::default(), &model, 100, 28);
    assert!(!rendered.contains('\u{202e}'));
    assert!(!rendered.contains('\u{1b}'));
}

#[test]
fn runtime_detail_sanitizes_and_bounds_remote_identity_text() {
    let unsafe_id = format!("runtime\u{202e}{}", "x".repeat(100));
    let mut board = BoardSnapshot::default();
    board.participants.insert(
        unsafe_id.clone(),
        ParticipantStatus::new(
            &unsafe_id,
            ParticipantKind::Service,
            ParticipantState::Ready,
        ),
    );
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let telemetry = TelemetrySnapshot::default();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
    let mut state = AppState::default();
    state.page = Page::Runtimes;
    state.navigation = NavigationLevel::Page;
    state.runtime_detail_id = Some(unsafe_id.clone());
    let rendered = render_model(&title(), &state, &model, 100, 28);
    assert!(!rendered.contains('\u{202e}'));
    assert!(!rendered.contains(&unsafe_id));
    assert!(rendered.contains("Identity"), "{rendered}");
}

#[test]
fn input_page_uses_devices_and_compact_motion_panels() {
    let rendered = render_page(Page::Input, &TelemetrySnapshot::default());
    assert!(rendered.contains("Devices"));
    assert!(rendered.contains("Motion"));
    assert!(rendered.contains("Command"));
    assert!(rendered.contains("Motion update"));
}

#[test]
fn small_terminal_degrades_to_resize_message() {
    let board = BoardSnapshot::default();
    let logs = LogView::new();
    let runtime = RuntimeView::new();
    let telemetry = TelemetrySnapshot::default();
    let model = SessionViewModel::new(&board, &logs, &runtime, &telemetry, Instant::now());
    let backend = TestBackend::new(30, 6);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut state = AppState::default();
    terminal
        .draw(|frame| {
            draw(
                frame,
                Theme::new(ColorCapability::None),
                &title(),
                &mut state,
                &model,
            );
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let text = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Terminal too small"));
}

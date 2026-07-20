//! Bus rendering responsibilities.

use super::*;

pub(super) fn draw_bus(
    frame: &mut Frame,
    theme: Theme,
    state: &mut AppState,
    model: &SessionViewModel<'_>,
    area: Rect,
) {
    let summary_height = (area.height / 3).clamp(5, 15);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(summary_height),
            Constraint::Min(1),
        ])
        .split(area);
    let sample = model.telemetry.router.as_ref();
    let freshness = sample.map_or_else(
        || "n/a".to_string(),
        |sample| {
            let age = model.now.saturating_duration_since(sample.received_at);
            if sample.is_stale(model.now, DEFAULT_FRESHNESS_TTL) {
                format!("stale · {} ago", human::duration(age))
            } else {
                format!("{} ago", human::duration(age))
            }
        },
    );
    let filter = CaseInsensitiveNeedle::new(&state.bus_filter);
    let reveal_internal = state.bus_show_internal;
    let controls = [
        format!("Filter: {}", empty_as_all(&state.bus_filter)),
        format!("Sort: {}", state.bus_sort.label()),
        format!(
            "Internal topics: {}",
            if reveal_internal { "Shown" } else { "Hidden" }
        ),
    ];
    let control_line = if area.width < 80 {
        let index = state.bus_control_cursor.min(controls.len() - 1);
        let style = if state.navigation == NavigationLevel::Page {
            color::candidate(theme, Role::Accent)
        } else {
            color::muted(theme)
        };
        Line::styled(
            format!(
                " {}/3 {} · router {} ",
                index + 1,
                controls[index],
                freshness
            ),
            style,
        )
    } else {
        Line::from(
            controls
                .into_iter()
                .enumerate()
                .flat_map(|(index, label)| {
                    let style = if state.navigation == NavigationLevel::Page
                        && state.bus_control_cursor == index
                    {
                        color::candidate(theme, Role::Accent)
                    } else {
                        color::muted(theme)
                    };
                    [Span::styled(format!(" {label} "), style), Span::raw("  ")]
                })
                .chain([Span::styled(
                    format!("Router freshness: {freshness}"),
                    color::muted(theme),
                )])
                .collect::<Vec<_>>(),
        )
    };
    frame.render_widget(
        Paragraph::new(vec![
            control_line,
            Line::styled(
                "←→ choose control · Enter edit/change · ↑↓ scroll topics",
                color::muted(theme),
            ),
        ]),
        rows[0],
    );
    let Some(sample) = sample else {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("Router telemetry has not arrived yet."),
                Line::from("Waiting for tool-bus on v2/router/metrics…"),
            ])
            .block(shell_block(theme, "Router status")),
            rows[1],
        );
        frame.render_widget(
            Paragraph::new("No topic traffic observed")
                .block(shell_block(theme, "Topics · Producer · Rate · Count")),
            rows[2],
        );
        return;
    };
    let mut topics = sample
        .value
        .topics
        .iter()
        .filter(|metric| state.bus_metric_matches(metric, model, &filter))
        .collect::<Vec<_>>();
    sort_topics(&mut topics, state.bus_sort);
    let summaries = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(rows[1]);
    let throughput_history = &model.telemetry.router_throughput_history;
    let spark_width = usize::from(summaries[0].width.saturating_sub(2));
    let visible_history_points = history_tail(throughput_history, spark_width);
    let history = visible_history_points
        .iter()
        .map(|point| (point.value.max(0.0) * 10.0).round() as u64)
        .collect::<Vec<_>>();
    let max = history.iter().copied().max().unwrap_or(1).max(1);
    let total_title = if sample.is_stale(model.now, DEFAULT_FRESHNESS_TTL) {
        format!(
            "Last known · {:.1} messages/s · stale · {}",
            sample.value.throughput_msg_s,
            history_span(visible_history_points),
        )
    } else {
        format!(
            "Total · {:.1} messages/s · {}",
            sample.value.throughput_msg_s,
            history_span(visible_history_points),
        )
    };
    frame.render_widget(
        Sparkline::default()
            .block(shell_block(theme, &total_title))
            .data(&history)
            .max(max)
            .style(color::fg(theme, Role::Accent)),
        summaries[0],
    );

    let mut producers = BTreeMap::<String, (f32, u64)>::new();
    // Keep this panel on the router's full detailed sample so its rates align
    // with the adjacent total as closely as the bounded wire table permits.
    // The overflow aggregate is not a producer and may double-count names
    // already present in detailed rows, so it is intentionally excluded.
    for metric in sample
        .value
        .topics
        .iter()
        .filter(|metric| !metric.aggregate_overflow)
    {
        let participant = if metric.from_participant.is_empty() {
            "unknown"
        } else {
            metric.from_participant.as_str()
        };
        let entry = producers.entry(participant.to_string()).or_default();
        entry.0 += metric.ingress_rate_hz;
        entry.1 = entry.1.saturating_add(metric.count);
    }
    let mut producers = producers.into_iter().collect::<Vec<_>>();
    producers.sort_by(|left, right| {
        right
            .1
            .0
            .total_cmp(&left.1.0)
            .then_with(|| left.0.cmp(&right.0))
    });
    let producer_inner_width = usize::from(summaries[1].width.saturating_sub(2));
    let producer_rate_width = if producer_inner_width >= 42 { 10 } else { 6 };
    let producer_count_width = if producer_inner_width >= 42 { 12 } else { 6 };
    let producer_name_width = producer_inner_width
        .saturating_sub(producer_rate_width + producer_count_width + 2)
        .max(1);
    let mut producer_lines = vec![Line::styled(
        format!(
            "{:<name_width$} {:>rate_width$} {:>count_width$}",
            fit_cell("PRODUCER", producer_name_width),
            ellipsize("MSG/S", producer_rate_width),
            ellipsize("COUNT", producer_count_width),
            name_width = producer_name_width,
            rate_width = producer_rate_width,
            count_width = producer_count_width,
        ),
        color::muted(theme),
    )];
    let has_overflow = sample
        .value
        .topics
        .iter()
        .any(|metric| metric.aggregate_overflow);
    if has_overflow {
        producer_lines.push(Line::styled(
            "Overflow excluded; total still includes it",
            color::muted(theme),
        ));
    }
    let available_rows = usize::from(summaries[1].height.saturating_sub(3))
        .saturating_sub(usize::from(has_overflow));
    let truncated = producers.len() > available_rows;
    let producer_limit = if truncated {
        available_rows.saturating_sub(1)
    } else {
        available_rows
    };
    producer_lines.extend(producers.iter().take(producer_limit).map(
        |(producer, (rate, count))| {
            let producer = sanitize_and_fit_cell(producer, producer_name_width);
            let rate = format!("{rate:.1}");
            Line::from(format!(
                "{producer} {rate:>producer_rate_width$} {count:>producer_count_width$}"
            ))
        },
    ));
    if truncated && available_rows > 0 {
        producer_lines.push(Line::styled(
            format!("… +{} more", producers.len().saturating_sub(producer_limit)),
            color::muted(theme),
        ));
    }
    if producers.is_empty() {
        producer_lines.push(Line::from("No producers observed"));
    }
    frame.render_widget(
        Paragraph::new(producer_lines).block(shell_block(
            theme,
            &format!(
                "All producers · {}/{} shown · internals included",
                producer_limit.min(producers.len()),
                producers.len()
            ),
        )),
        summaries[1],
    );

    let topic_inner_width = usize::from(rows[2].width.saturating_sub(2));
    let topic_rate_width = 9;
    let topic_count_width = if topic_inner_width >= 70 { 9 } else { 6 };
    let topic_text_width = topic_inner_width
        .saturating_sub(topic_rate_width + topic_count_width + 3)
        .max(2);
    let topic_producer_width = (topic_text_width / 3).clamp(8, 20).min(topic_text_width);
    let topic_name_width = topic_text_width.saturating_sub(topic_producer_width).max(1);
    let height = usize::from(rows[2].height.saturating_sub(3));
    state.bus_scroll = bounded_window_start(state.bus_scroll, topics.len(), height);
    let start = state.bus_scroll;
    let mut lines = vec![Line::from(format!(
        "{:<topic_width$} {:<producer_width$} {:>rate_width$} {:>count_width$}",
        fit_cell("TOPIC", topic_name_width),
        fit_cell("PRODUCER", topic_producer_width),
        "RATE",
        "COUNT",
        topic_width = topic_name_width,
        producer_width = topic_producer_width,
        rate_width = topic_rate_width,
        count_width = topic_count_width,
    ))];
    lines.extend(topics.iter().skip(start).take(height).map(|metric| {
        let topic = sanitize_and_fit_cell(&metric.topic, topic_name_width);
        let producer = if metric.aggregate_overflow {
            fit_cell("aggregate", topic_producer_width)
        } else if metric.from_participant.is_empty() {
            fit_cell("unknown", topic_producer_width)
        } else {
            sanitize_and_fit_cell(&metric.from_participant, topic_producer_width)
        };
        let rate = format!("{:.1} Hz", metric.ingress_rate_hz);
        Line::from(format!(
            "{topic} {producer} {rate:>topic_rate_width$} {:>topic_count_width$}",
            metric.count
        ))
    }));
    let shown = topics.len().saturating_sub(start).min(height);
    let visible_end = start.saturating_add(shown);
    let truncation = if sample.value.topics_truncated == 0 {
        String::new()
    } else {
        format!(" · +{} omitted", sample.value.topics_truncated)
    };
    let topic_title = format!(
        "Topics · {}/{} visible · {}-{} of {}{truncation}",
        topics.len(),
        sample.value.topics.len(),
        if shown == 0 { 0 } else { start + 1 },
        visible_end,
        topics.len(),
    );
    frame.render_widget(
        Paragraph::new(lines).block(shell_block(theme, &topic_title)),
        rows[2],
    );
}

pub(super) fn bounded_window_start(
    offset: usize,
    item_count: usize,
    window_height: usize,
) -> usize {
    offset.min(item_count.saturating_sub(window_height))
}

pub(super) fn history_tail<T>(history: &[T], width: usize) -> &[T] {
    &history[history.len().saturating_sub(width)..]
}

pub(super) fn history_span(history: &[Timestamped<f32>]) -> String {
    match (history.first(), history.last()) {
        (None, _) => "waiting".to_string(),
        (Some(_), Some(_)) if history.len() == 1 => "1 sample".to_string(),
        (Some(first), Some(last)) => format!(
            "last {}",
            human::duration(
                last.received_at
                    .saturating_duration_since(first.received_at)
            )
        ),
        _ => "waiting".to_string(),
    }
}

pub(super) fn sort_topics(topics: &mut [&TopicMetric], sort: BusSort) {
    match sort {
        BusSort::Rate => topics.sort_by(|left, right| {
            right
                .ingress_rate_hz
                .total_cmp(&left.ingress_rate_hz)
                .then_with(|| left.topic.cmp(&right.topic))
        }),
        BusSort::Topic => topics.sort_by(|left, right| left.topic.cmp(&right.topic)),
        BusSort::Producer => topics.sort_by(|left, right| {
            left.from_participant
                .cmp(&right.from_participant)
                .then_with(|| left.topic.cmp(&right.topic))
        }),
    }
}

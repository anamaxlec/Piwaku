//! PIWAKU: native task panel fed by rpiv-todo snapshots.
//!
//! The panel is persistent session state rendered above the composer — never
//! transcript output — and mirrors the agent's own task list verbatim from
//! the latest `todo` tool snapshot. Collapse is an app-level preference.

use super::*;

impl Waku {
    pub(super) fn render_todo_panel(&self, cx: &mut Context<Self>) -> Option<Div> {
        let snapshot = self.selected_runtime()?.todo_state.as_ref()?;
        let tasks: Vec<&TodoTask> = snapshot.visible_tasks().collect();
        if tasks.is_empty() {
            return None;
        }
        let theme = Theme::current(cx);
        let completed = snapshot.completed_count();
        let total = tasks.len();

        let header_focus = self.transcript_control_focus("todo-panel-header", cx);
        let header = div()
            .id("todo-panel-header")
            .track_focus(&header_focus)
            .tab_index(0)
            .tab_stop(true)
            .flex()
            .items_center()
            .gap(px(7.0))
            .cursor_default()
            .focus_visible(|style| style.border_1().border_color(theme.accent))
            .hover(|style| style.bg(theme.overlay))
            .on_click(cx.listener(|this, _, _, cx| this.toggle_todo_panel(cx)))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.toggle_todo_panel(cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                div()
                    .text_size(sp(12.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text_secondary)
                    .child(tr!("todo_panel.title")),
            )
            .child(
                div()
                    .h(px(17.0))
                    .px(px(6.0))
                    .rounded(px(5.0))
                    .bg(theme.overlay)
                    .flex()
                    .items_center()
                    .text_size(sp(11.5))
                    .text_color(theme.text_tertiary)
                    .child(tr!(
                        "todo_panel.progress",
                        completed = completed,
                        total = total
                    )),
            )
            .child(div().flex_1())
            .child(icon(
                if self.todo_panel_collapsed {
                    "icons/chevron-right.svg"
                } else {
                    "icons/chevron-down.svg"
                },
                12.0,
                theme.text_tertiary,
            ));

        let mut body = div().mt(px(6.0)).flex().flex_col().gap(px(3.0));
        if !self.todo_panel_collapsed {
            for task in &tasks {
                body = body.child(render_todo_row(task, &theme));
            }
        }

        Some(
            div().flex_none().px(px(20.0)).pb(px(6.0)).child(
                div()
                    .w_full()
                    .max_w(px(CONTENT_MAX_WIDTH))
                    .mx_auto()
                    .rounded(px(11.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.composer)
                    .px(px(12.0))
                    .py(px(8.0))
                    .child(header)
                    .child(body),
            ),
        )
    }

    pub(super) fn toggle_todo_panel(&mut self, cx: &mut Context<Self>) {
        self.todo_panel_collapsed = !self.todo_panel_collapsed;
        cx.notify();
    }
}

fn render_todo_row(task: &TodoTask, theme: &Theme) -> Div {
    let (marker, subject_color, weight) = match task.status {
        TodoTaskStatus::InProgress => (
            icon("icons/loader-circle.svg", 11.0, theme.accent).into_any_element(),
            theme.text,
            FontWeight::MEDIUM,
        ),
        TodoTaskStatus::Completed => (
            icon("icons/check.svg", 11.0, theme.text_tertiary).into_any_element(),
            theme.text_tertiary,
            FontWeight::MEDIUM,
        ),
        // A hollow ring keeps pending rows visually lighter than active work.
        TodoTaskStatus::Pending => (
            div()
                .size(px(9.0))
                .rounded_full()
                .border_1()
                .border_color(theme.text_ghost)
                .into_any_element(),
            theme.text_secondary,
            FontWeight::MEDIUM,
        ),
        TodoTaskStatus::Deleted => unreachable!("deleted tasks are filtered before rendering"),
    };

    let mut row = div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .min_h(px(20.0))
        .py(px(1.0))
        .child(marker)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .text_size(sp(12.0))
                        .line_height(sp(16.0))
                        .font_weight(weight)
                        .text_color(subject_color)
                        .when(task.status == TodoTaskStatus::Completed, |subject| {
                            subject.line_height(sp(15.0)).opacity(0.75)
                        })
                        .child(SharedString::from(task.subject.clone())),
                )
                .children(task.active_form.as_ref().and_then(|active_form| {
                    (task.status == TodoTaskStatus::InProgress).then(|| {
                        div()
                            .text_size(sp(11.5))
                            .line_height(sp(14.0))
                            .text_color(theme.accent.opacity(0.85))
                            .child(SharedString::from(active_form.clone()))
                    })
                })),
        );

    if !task.blocked_by.is_empty() {
        row = row.child(lock(task, theme));
    }
    row
}

fn lock(task: &TodoTask, theme: &Theme) -> AnyElement {
    let ids = task
        .blocked_by
        .iter()
        .map(|id| format!("#{id}"))
        .collect::<Vec<_>>()
        .join(" ");
    div()
        .flex()
        .items_center()
        .gap(px(3.0))
        .child(icon("icons/lock.svg", 9.0, theme.text_ghost))
        .child(
            div()
                .text_size(sp(11.0))
                .text_color(theme.text_ghost)
                .child(SharedString::from(ids)),
        )
        .into_any_element()
}

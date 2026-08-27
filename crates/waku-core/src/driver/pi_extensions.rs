//! Pi extension UI dialog bridge.
//!
//! Pi extensions ask for user input through `ctx.ui.select` / `confirm` /
//! `input` / `editor`, which RPC mode surfaces as
//! `{"type":"extension_ui_request", id, method, ...}` frames expecting one
//! `extension_ui_response` back. Waku normalizes each request into the
//! provider-neutral [`UserInputQuestion`] model so the desktop question panel
//! renders it natively, and translates the panel's answers back into the
//! exact response shape Pi's pending-request table resolves.
//!
//! Wire contract verified against pi 0.84.2 (`dist/modes/rpc/rpc-mode.js`):
//! select responses echo the chosen option string verbatim, confirm answers
//! `{confirmed: bool}`, input/editor answer `{value: string}`, and any request
//! may instead be answered `{cancelled: true}`.

use serde_json::{Value, json};

use crate::model::{
    GoalOperation, ThreadGoal, ThreadGoalStatus, TodoSnapshot, TodoTask, TodoTaskStatus,
    UserInputAnswer, UserInputOption, UserInputQuestion,
};

/// The dialog primitives this bridge understands. Fire-and-forget requests
/// (`notify`, `setStatus`, `setWidget`, `setTitle`, `set_editor_text`) are
/// deliberately absent: they expect no response and are ignored upstream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtensionUiMethod {
    Select,
    Confirm,
    Input,
    Editor,
}

impl ExtensionUiMethod {
    fn from_wire(method: &str) -> Option<Self> {
        match method {
            "select" => Some(Self::Select),
            "confirm" => Some(Self::Confirm),
            "input" => Some(Self::Input),
            "editor" => Some(Self::Editor),
            _ => None,
        }
    }
}

/// What the writer thread needs to translate a panel answer back into the
/// response frame Pi expects.
#[derive(Clone, Debug)]
pub(crate) struct PendingExtensionUi {
    pub(crate) method: ExtensionUiMethod,
    /// Option labels as issued. For confirms these are the localized OK /
    /// Cancel labels in order, so the boolean never depends on parsing text.
    pub(crate) option_labels: Vec<String>,
}

impl PendingExtensionUi {
    /// Whether the answer set declines every question. An empty submission
    /// (the panel submitted without any selection or typed text) cancels the
    /// request rather than fabricating an empty value.
    fn is_cancelled(answers: &[UserInputAnswer]) -> bool {
        answers
            .iter()
            .all(|answer| answer.answers.iter().all(|entry| entry.trim().is_empty()))
    }

    fn primary_answer(answers: &[UserInputAnswer]) -> Option<&str> {
        let trimmed = answers.first()?.answers.first()?.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    }

    /// Build the complete `extension_ui_response` frame for this request.
    /// Returns `None` when the answer shape cannot be interpreted, which
    /// callers treat as "leave the request unanswered" rather than sending a
    /// corrupt frame. Pi routes inbound lines by `type` and resolves the
    /// pending promise by `id`, so both ride on every frame.
    pub(crate) fn build_response(
        &self,
        request_id: &str,
        answers: &[UserInputAnswer],
    ) -> Option<Value> {
        if Self::is_cancelled(answers) {
            return Some(self.frame(request_id, json!({ "cancelled": true })));
        }
        let fields = match self.method {
            // Pi's select returns the chosen string verbatim; hosts echoing a
            // string outside the offered list are indistinguishable from a
            // dismissal on the extension side, so passing custom text through
            // is safe even though it reads as a decline there.
            ExtensionUiMethod::Select | ExtensionUiMethod::Input | ExtensionUiMethod::Editor => {
                json!({ "value": Self::primary_answer(answers)? })
            }
            ExtensionUiMethod::Confirm => {
                let label = Self::primary_answer(answers)?;
                let confirmed = self.option_labels.first().is_some_and(|ok| ok == label);
                json!({ "confirmed": confirmed })
            }
        };
        Some(self.frame(request_id, fields))
    }

    fn frame(&self, request_id: &str, mut fields: Value) -> Value {
        if let Some(object) = fields.as_object_mut() {
            object.insert("type".to_owned(), json!("extension_ui_response"));
            object.insert("id".to_owned(), json!(request_id));
        }
        fields
    }
}

/// PIWAKU: pi-goal persists its state as `goal-state` custom session
/// entries — the LAST one on the branch is authoritative, mirroring the
/// plugin's own `loadGoalStateFromSession`. The session file path is the
/// one the driver already tracks for resume; reading it here means the
/// goal UI reflects exactly what the plugin wrote, with no protocol
/// changes and no state owned in two places.
pub(crate) fn read_goal_from_session_file(session_file: &std::path::Path) -> Option<ThreadGoal> {
    let content = std::fs::read_to_string(session_file).ok()?;
    let goal = content
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|entry| entry.get("customType").and_then(Value::as_str) == Some("goal-state"))?;
    let goal = goal.pointer("/data/goal")?;
    if goal.is_null() {
        return None;
    }
    let objective = goal.get("text").and_then(Value::as_str)?.trim();
    if objective.is_empty() {
        return None;
    }
    let status = match goal.get("status").and_then(Value::as_str)? {
        // `queued` is pi-goal's legacy queue state; Waku has no equivalent.
        "active" => ThreadGoalStatus::Active,
        "paused" => ThreadGoalStatus::Paused,
        "blocked" => ThreadGoalStatus::Blocked,
        "usage_limited" => ThreadGoalStatus::UsageLimited,
        "budget_limited" => ThreadGoalStatus::BudgetLimited,
        "complete" => ThreadGoalStatus::Complete,
        _ => return None,
    };
    Some(ThreadGoal {
        objective: objective.to_owned(),
        status,
        token_budget: goal.get("tokenBudget").and_then(Value::as_i64),
        tokens_used: goal.get("tokensUsed").and_then(Value::as_i64).unwrap_or(0),
        time_used_seconds: goal
            .get("timeUsedSeconds")
            .and_then(Value::as_i64)
            .unwrap_or(0),
    })
}

/// PIWAKU: goal mutations route through the plugin's own `/goal` command —
/// it owns the runtime (continuation prompts, budgets, safety) and writing
/// session entries behind its back would be clobbered on the next
/// `persistGoal`. `None` means the operation needs no prompt (Refresh is
/// served by re-reading the session file).
pub(crate) fn goal_prompt_for_operation(operation: &GoalOperation) -> Option<String> {
    match operation {
        GoalOperation::Refresh => None,
        GoalOperation::Clear => Some("/goal clear".to_owned()),
        GoalOperation::Set {
            objective, status, ..
        } => {
            if let Some(objective) = objective
                .as_deref()
                .map(str::trim)
                .filter(|objective| !objective.is_empty())
            {
                // Starting an objective replaces the previous one and
                // kicks off goal mode — exactly what "set a goal" means.
                return Some(format!("/goal {objective}"));
            }
            match status {
                Some(ThreadGoalStatus::Paused) => Some("/goal pause".to_owned()),
                Some(ThreadGoalStatus::Active) => Some("/goal resume".to_owned()),
                _ => None,
            }
        }
    }
}

/// PIWAKU: rebuild the agent's task list from a successful `todo` tool
/// result. rpiv-todo returns the complete snapshot under `details` on every
/// call, so this never diffs — invalid entries are skipped individually and
/// `None` means the payload carried no list at all (leave current state).
pub(crate) fn parse_todo_snapshot(result: &Value) -> Option<TodoSnapshot> {
    let details = result.get("details")?;
    let entries = details.get("tasks")?.as_array()?;
    let mut tasks = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(id) = entry.get("id").and_then(Value::as_u64) else {
            continue;
        };
        let Some(subject) = entry
            .get("subject")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        let Some(status) = entry.get("status").and_then(Value::as_str) else {
            continue;
        };
        let status = match status {
            "pending" => TodoTaskStatus::Pending,
            "in_progress" => TodoTaskStatus::InProgress,
            "completed" => TodoTaskStatus::Completed,
            "deleted" => TodoTaskStatus::Deleted,
            _ => continue,
        };
        let description = entry
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let active_form = entry
            .get("activeForm")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let blocked_by = entry
            .get("blockedBy")
            .and_then(Value::as_array)
            .map(|ids| ids.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default();
        tasks.push(TodoTask {
            id,
            subject,
            description,
            active_form,
            status,
            blocked_by,
        });
    }
    let next_id = details.get("nextId").and_then(Value::as_u64)?;
    Some(TodoSnapshot { tasks, next_id })
}

/// A normalized inbound request: everything needed to emit one
/// `DriverEvent::UserInputRequested` and remember how to answer it.
pub(crate) struct ParsedExtensionUiRequest {
    pub(crate) id: String,
    pub(crate) pending: PendingExtensionUi,
    pub(crate) questions: Vec<UserInputQuestion>,
}

/// Normalize an `extension_ui_request` frame into native question-panel state.
///
/// Every Pi dialog primitive carries exactly one question, so each request
/// becomes a single-question panel step. `None` means the frame is not an
/// interactive dialog (fire-and-forget methods, unknown methods, malformed
/// payloads) and must be ignored rather than answered.
pub(crate) fn parse_extension_ui_request(value: &Value) -> Option<ParsedExtensionUiRequest> {
    let method = ExtensionUiMethod::from_wire(value.get("method").and_then(Value::as_str)?)?;
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)?;
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    let (question_text, option_labels): (String, Vec<String>) = match method {
        ExtensionUiMethod::Select => {
            let options: Vec<String> = value
                .get("options")
                .and_then(Value::as_array)
                .map(|entries| {
                    entries
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            if options.is_empty() {
                return None;
            }
            (title.to_owned(), options)
        }
        ExtensionUiMethod::Confirm => {
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|message| !message.is_empty());
            let text = match message {
                Some(message) => format!("{title}\n\n{message}"),
                None => title.to_owned(),
            };
            (
                text,
                vec![
                    tr!("user_input.confirm_ok"),
                    tr!("user_input.confirm_cancel"),
                ],
            )
        }
        ExtensionUiMethod::Input | ExtensionUiMethod::Editor => (title.to_owned(), Vec::new()),
    };
    if question_text.is_empty() {
        return None;
    }
    let questions = vec![UserInputQuestion {
        id: id.clone(),
        header: String::new(),
        question: question_text,
        options: option_labels
            .iter()
            .map(|label| UserInputOption {
                label: label.clone(),
                description: None,
            })
            .collect(),
        multi_select: false,
    }];
    Some(ParsedExtensionUiRequest {
        pending: PendingExtensionUi {
            method,
            option_labels,
        },
        id,
        questions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_state_reads_the_last_session_entry() {
        let dir = std::env::temp_dir().join(format!("piwaku-goal-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("session.jsonl");
        std::fs::write(
            &file,
            [
                r#"{"type":"user","text":"hi"}"#,
                r#"{"customType":"goal-state","data":{"goal":{"id":"g1","text":"ship it","status":"active","tokensUsed":120,"timeUsedSeconds":30}}}"#,
                r#"{"type":"assistant","text":"ok"}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        let goal = read_goal_from_session_file(&file).expect("goal found");
        assert_eq!(goal.objective, "ship it");
        assert_eq!(goal.status, ThreadGoalStatus::Active);
        assert_eq!(goal.tokens_used, 120);
        assert_eq!(goal.time_used_seconds, 30);

        // A later cleared entry (goal: null) wins — the goal is gone.
        std::fs::write(
            &file,
            [
                r#"{"customType":"goal-state","data":{"goal":{"id":"g1","text":"ship it","status":"active"}}}"#,
                r#"{"customType":"goal-state","data":{"goal":null}}"#,
            ]
            .join("\n"),
        )
        .unwrap();
        assert_eq!(read_goal_from_session_file(&file), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn goal_operations_map_to_the_plugin_command() {
        assert_eq!(
            goal_prompt_for_operation(&GoalOperation::Refresh),
            None,
            "refresh re-reads the file instead of prompting"
        );
        assert_eq!(
            goal_prompt_for_operation(&GoalOperation::Clear),
            Some("/goal clear".to_owned())
        );
        assert_eq!(
            goal_prompt_for_operation(&GoalOperation::Set {
                objective: Some("write the report".to_owned()),
                status: None,
                replace: true,
            }),
            Some("/goal write the report".to_owned())
        );
        assert_eq!(
            goal_prompt_for_operation(&GoalOperation::Set {
                objective: None,
                status: Some(ThreadGoalStatus::Paused),
                replace: false,
            }),
            Some("/goal pause".to_owned())
        );
        assert_eq!(
            goal_prompt_for_operation(&GoalOperation::Set {
                objective: None,
                status: Some(ThreadGoalStatus::Active),
                replace: false,
            }),
            Some("/goal resume".to_owned())
        );
    }

    fn answers(values: &[&str]) -> Vec<UserInputAnswer> {
        vec![UserInputAnswer {
            question_id: "req-1".into(),
            answers: values.iter().map(|value| value.to_string()).collect(),
        }]
    }

    #[test]
    fn select_request_normalizes_options_verbatim() {
        let parsed = parse_extension_ui_request(&json!({
            "type": "extension_ui_request",
            "id": "abc",
            "method": "select",
            "title": "Pick one",
            "options": ["1. Alpha — a", "2. Beta — b"]
        }))
        .expect("select parses");
        assert_eq!(parsed.id, "abc");
        assert_eq!(parsed.pending.method, ExtensionUiMethod::Select);
        assert_eq!(parsed.questions.len(), 1);
        let question = &parsed.questions[0];
        assert_eq!(question.question, "Pick one");
        assert_eq!(
            question
                .options
                .iter()
                .map(|option| option.label.as_str())
                .collect::<Vec<_>>(),
            ["1. Alpha — a", "2. Beta — b"]
        );
    }

    #[test]
    fn confirm_folds_message_into_question_and_pins_labels() {
        let parsed = parse_extension_ui_request(&json!({
            "type": "extension_ui_request",
            "id": "c1",
            "method": "confirm",
            "title": "Delete branch?",
            "message": "This cannot be undone"
        }))
        .expect("confirm parses");
        assert_eq!(parsed.pending.method, ExtensionUiMethod::Confirm);
        assert_eq!(parsed.pending.option_labels.len(), 2);
        assert_eq!(
            parsed.questions[0].question,
            "Delete branch?\n\nThis cannot be undone"
        );
    }

    #[test]
    fn input_has_no_options_so_the_panel_uses_free_text() {
        let parsed = parse_extension_ui_request(&json!({
            "type": "extension_ui_request",
            "id": "i1",
            "method": "input",
            "title": "Session name",
            "placeholder": "my-session"
        }))
        .expect("input parses");
        assert!(parsed.questions[0].options.is_empty());
    }

    #[test]
    fn fire_and_forget_methods_are_ignored() {
        for method in [
            "notify",
            "setStatus",
            "setWidget",
            "setTitle",
            "set_editor_text",
            "custom",
        ] {
            assert!(
                parse_extension_ui_request(&json!({
                    "type": "extension_ui_request",
                    "id": "x",
                    "method": method
                }))
                .is_none(),
                "{method} must not become a question"
            );
        }
    }

    #[test]
    fn select_response_echoes_label_and_cancel_maps_to_cancelled() {
        let pending = PendingExtensionUi {
            method: ExtensionUiMethod::Select,
            option_labels: vec!["1. Alpha — a".into(), "2. Beta — b".into()],
        };
        let response = pending
            .build_response("req-9", &answers(&["1. Alpha — a"]))
            .unwrap();
        assert_eq!(
            response,
            json!({ "type": "extension_ui_response", "id": "req-9", "value": "1. Alpha — a" })
        );

        let cancelled = pending.build_response("req-9", &answers(&[])).unwrap();
        assert_eq!(
            cancelled,
            json!({ "type": "extension_ui_response", "id": "req-9", "cancelled": true })
        );
    }

    #[test]
    fn confirm_response_maps_by_option_position_not_locale_parsing() {
        let pending = PendingExtensionUi {
            method: ExtensionUiMethod::Confirm,
            option_labels: vec!["确认".into(), "取消".into()],
        };
        assert_eq!(
            pending.build_response("c1", &answers(&["确认"])).unwrap(),
            json!({ "type": "extension_ui_response", "id": "c1", "confirmed": true })
        );
        assert_eq!(
            pending.build_response("c1", &answers(&["取消"])).unwrap(),
            json!({ "type": "extension_ui_response", "id": "c1", "confirmed": false })
        );
    }

    #[test]
    fn blank_answers_cancel_instead_of_sending_empty_values() {
        let pending = PendingExtensionUi {
            method: ExtensionUiMethod::Input,
            option_labels: Vec::new(),
        };
        assert_eq!(
            pending.build_response("i1", &answers(&["  "])).unwrap(),
            json!({ "type": "extension_ui_response", "id": "i1", "cancelled": true })
        );
        assert_eq!(
            pending
                .build_response("i1", &answers(&["typed answer"]))
                .unwrap(),
            json!({ "type": "extension_ui_response", "id": "i1", "value": "typed answer" })
        );
    }

    #[test]
    fn todo_snapshots_parse_from_tool_results() {
        let result = json!({
            "content": [{ "type": "text", "text": "ok" }],
            "details": {
                "action": "update",
                "params": {},
                "nextId": 4,
                "tasks": [
                    { "id": 1, "subject": "Done task", "status": "completed" },
                    { "id": 2, "subject": "Active task", "status": "in_progress", "activeForm": "writing tests", "blockedBy": [1] },
                    { "id": 3, "subject": "Pending task", "status": "pending", "description": "later" },
                    { "id": 9, "subject": "Gone", "status": "deleted" },
                    { "id": "bad", "subject": "Broken", "status": "pending" },
                    { "subject": "No id", "status": "pending" }
                ]
            }
        });
        let snapshot = parse_todo_snapshot(&result).expect("parses");
        assert_eq!(snapshot.next_id, 4);
        let tasks: Vec<_> = snapshot.visible_tasks().collect();
        assert_eq!(tasks.len(), 3);
        assert_eq!(snapshot.completed_count(), 1);
        assert_eq!(tasks[1].active_form.as_deref(), Some("writing tests"));
        assert_eq!(tasks[1].blocked_by, vec![1]);
        assert_eq!(tasks[2].description.as_deref(), Some("later"));

        // No details / no tasks key → leave current state alone.
        assert_eq!(parse_todo_snapshot(&json!({})), None);
        assert_eq!(parse_todo_snapshot(&json!({ "details": {} })), None);
    }
}

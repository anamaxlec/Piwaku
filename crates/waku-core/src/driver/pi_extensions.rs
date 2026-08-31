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

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};

use serde_json::{Value, json};

use crate::model::{
    GoalOperation, InteractionMode, MagicContextStatus, PermissionOption, ThreadGoal,
    ThreadGoalStatus, TodoSnapshot, TodoTask, TodoTaskStatus, UserInputAnswer, UserInputOption,
    UserInputQuestion,
};

const MAGIC_STATUS_MAX_CHARS: usize = 512;
const MAGIC_PERSISTED_STATUS_MAX_CHARS: usize = 16 * 1024;
const MAGIC_SESSION_MAX_BYTES: u64 = 512 * 1024;
const MAGIC_SESSION_MAX_ENTRIES: usize = 2048;

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
    /// Permission selects are answered through `DriverControl::respond`;
    /// other extension dialogs use `respond_user_input`.
    pub(crate) permission: bool,
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

/// The permission-system fallback has one deliberately narrow wire shape.
/// Keep this predicate strict so an unrelated extension select is still
/// exposed as a structured question rather than a permission decision.
fn is_permission_select_options(options: &[String]) -> bool {
    options.len() == 4
        && options[0] == "Yes"
        && !options[1].trim().is_empty()
        && options[2] == "No"
        && options[3] == "No, provide reason"
}

/// A permission-system `select()` request normalized for Waku's native
/// permission panel. The original labels are also kept in `pending` for the
/// later response bridge.
pub(crate) struct ParsedPermissionRequest {
    pub(crate) id: String,
    pub(crate) pending: PendingExtensionUi,
    pub(crate) title: String,
    pub(crate) detail: String,
    pub(crate) options: Vec<PermissionOption>,
}

/// Fire-and-forget UI requests emitted by Pi's RPC mode. `notify` is generic
/// Pi extension traffic; only the exact `magic-context` status key is Magic's.
/// They are kept out of the interactive-dialog parser because Pi never expects
/// a response frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ParsedPiUiRequest {
    Notify { message: String, level: String },
    Status { text: Option<String> },
}

/// Parse bounded Pi fire-and-forget UI requests. A `notify` is exposed as a
/// provider-neutral notification; `setStatus` is accepted only for Magic's
/// exact status key.
pub(crate) fn parse_pi_ui_request(value: &Value) -> Option<ParsedPiUiRequest> {
    match value.get("method").and_then(Value::as_str)? {
        "notify" => {
            let message = bounded_magic_single_line(value.get("message")?.as_str()?)?;
            let level = value
                .get("notifyType")
                .and_then(Value::as_str)
                .unwrap_or("info");
            if !matches!(level, "info" | "warning" | "error") {
                return None;
            }
            Some(ParsedPiUiRequest::Notify {
                message,
                level: level.to_owned(),
            })
        }
        "setStatus" => {
            if value.get("statusKey").and_then(Value::as_str) != Some("magic-context") {
                return None;
            }
            let text = match value.get("statusText") {
                None | Some(Value::Null) => None,
                Some(Value::String(text)) => {
                    let text = text.trim();
                    if text.is_empty() {
                        None
                    } else {
                        Some(bounded_magic_single_line(text)?)
                    }
                }
                _ => return None,
            };
            Some(ParsedPiUiRequest::Status { text })
        }
        _ => None,
    }
}

fn bounded_magic_single_line(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty()
        || text.chars().count() > MAGIC_STATUS_MAX_CHARS
        || text.chars().any(char::is_control)
    {
        return None;
    }
    Some(text.to_owned())
}

fn bounded_magic_multiline(text: &str) -> Option<String> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let text = normalized.trim();
    if text.is_empty()
        || text.chars().count() > MAGIC_PERSISTED_STATUS_MAX_CHARS
        || text
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return None;
    }
    Some(text.to_owned())
}

/// Read the latest Magic Context status on Pi's current session branch. The
/// session is an append-only tree, so follow the last persisted leaf through
/// `parentId` rather than treating an abandoned branch's later line as live.
pub(crate) fn read_magic_status_from_session_file(
    session_file: &std::path::Path,
) -> Option<MagicContextStatus> {
    let mut file = std::fs::File::open(session_file).ok()?;
    let file_len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(
        file_len.saturating_sub(MAGIC_SESSION_MAX_BYTES),
    ))
    .ok()?;
    let mut bytes = Vec::new();
    file.take(MAGIC_SESSION_MAX_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    let content = String::from_utf8_lossy(&bytes);
    let entries = content
        .lines()
        .rev()
        .take(MAGIC_SESSION_MAX_ENTRIES)
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect::<Vec<_>>();
    let leaf_id = entries
        .iter()
        .find_map(|entry| entry.get("id").and_then(Value::as_str))?;
    let by_id = entries
        .iter()
        .filter_map(|entry| Some((entry.get("id")?.as_str()?, entry)))
        .collect::<HashMap<_, _>>();
    let mut current_id = Some(leaf_id);
    let mut visited = HashSet::new();
    while let Some(id) = current_id {
        if !visited.insert(id) {
            break;
        }
        let entry = by_id.get(id).copied()?;
        if let Some(status) = parse_magic_status_entry(entry) {
            return Some(status);
        }
        current_id = match entry.get("parentId") {
            None | Some(Value::Null) => None,
            Some(parent_id) => parent_id.as_str(),
        };
    }
    None
}

fn parse_magic_status_entry(entry: &Value) -> Option<MagicContextStatus> {
    if entry.get("type").and_then(Value::as_str) != Some("custom")
        || entry.get("customType").and_then(Value::as_str) != Some("ctx-status")
    {
        return None;
    }
    let data = entry.get("data")?.as_object()?;
    let title = bounded_magic_single_line(data.get("title")?.as_str()?)?;
    let text = bounded_magic_multiline(data.get("text")?.as_str()?)?;
    let level = data.get("level")?.as_str()?;
    if !matches!(level, "info" | "success" | "warning" | "error") {
        return None;
    }
    Some(MagicContextStatus {
        title,
        text,
        level: level.to_owned(),
    })
}

/// Recognize the RPC fallback emitted by @gotgenes/pi-permission-system.
///
/// The fallback calls `ui.select(`${title}\n${renderedPayload}`, options)`;
/// therefore the first line is the stable permission heading and the rest is
/// the detail rendered for the native card. Only the two known headings and
/// the four exact decision labels are accepted.
pub(crate) fn parse_permission_request(value: &Value) -> Option<ParsedPermissionRequest> {
    if value.get("method").and_then(Value::as_str) != Some("select") {
        return None;
    }
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)?;
    let raw_title = value.get("title").and_then(Value::as_str)?;
    let (title, detail) = raw_title
        .split_once('\n')
        .map_or((raw_title, ""), |(title, detail)| (title, detail.trim()));
    if !matches!(
        title,
        "Permission Required" | "Permission Required (Subagent)"
    ) {
        return None;
    }
    if detail.is_empty() {
        return None;
    }
    let option_labels = value
        .get("options")
        .and_then(Value::as_array)?
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<_>>>()?
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if !is_permission_select_options(&option_labels) {
        return None;
    }

    Some(ParsedPermissionRequest {
        id,
        pending: PendingExtensionUi {
            method: ExtensionUiMethod::Select,
            option_labels: option_labels.clone(),
            permission: true,
        },
        title: title.to_owned(),
        detail: detail.to_owned(),
        options: vec![
            PermissionOption {
                id: option_labels[0].clone(),
                label: option_labels[0].clone(),
                allow: true,
            },
            PermissionOption {
                id: option_labels[1].clone(),
                label: option_labels[1].clone(),
                allow: true,
            },
            PermissionOption {
                id: option_labels[2].clone(),
                label: option_labels[2].clone(),
                allow: false,
            },
            PermissionOption {
                id: option_labels[3].clone(),
                label: option_labels[3].clone(),
                allow: false,
            },
        ],
    })
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

/// PIWAKU: pi-plan-mode persists the provider-owned mode as `custom`
/// `plan-mode-state` session entries. The last matching entry is authoritative;
/// an invalid or missing `data.enabled` means the plugin's default Build mode.
pub(crate) fn read_plan_mode_from_session_file(
    session_file: &std::path::Path,
) -> Option<InteractionMode> {
    let content = std::fs::read_to_string(session_file).ok()?;
    let entry = content
        .lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|entry| {
            entry.get("type").and_then(Value::as_str) == Some("custom")
                && entry.get("customType").and_then(Value::as_str) == Some("plan-mode-state")
        });
    let enabled = entry
        .and_then(|entry| entry.pointer("/data/enabled").and_then(Value::as_bool))
        .unwrap_or(false);
    Some(if enabled {
        InteractionMode::Plan
    } else {
        InteractionMode::Build
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

/// Read the latest successful `todo` tool result from Pi's active-branch
/// session entries.  `get_entries` returns the persisted message envelope,
/// while [`parse_todo_snapshot`] remains the single task-payload parser used
/// by both live tool events and this hydration path.
pub(crate) fn parse_latest_todo_snapshot_from_entries(response: &Value) -> Option<TodoSnapshot> {
    let entries = response
        .pointer("/data/entries")
        .and_then(Value::as_array)?;
    let leaf_id = response.pointer("/data/leafId").and_then(Value::as_str)?;
    if leaf_id.is_empty() {
        return None;
    }
    let by_id = entries
        .iter()
        .filter_map(|entry| Some((entry.get("id")?.as_str()?, entry)))
        .collect::<HashMap<_, _>>();
    let mut branch = Vec::new();
    let mut current_id = Some(leaf_id);
    let mut visited = HashSet::new();
    while let Some(id) = current_id {
        if !visited.insert(id) {
            return None;
        }
        let entry = by_id.get(id).copied()?;
        branch.push(entry);
        current_id = match entry.get("parentId") {
            None | Some(Value::Null) => None,
            Some(parent_id) => Some(parent_id.as_str()?),
        };
    }
    // The walk starts at leafId, so this is already newest-to-oldest within
    // the active branch.
    branch.into_iter().find_map(|entry| {
        let message = entry.get("message")?;
        if message.get("role").and_then(Value::as_str) != Some("toolResult")
            || message.get("toolName").and_then(Value::as_str) != Some("todo")
            || message.get("isError").and_then(Value::as_bool) == Some(true)
        {
            return None;
        }
        parse_todo_snapshot(message)
    })
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
            permission: false,
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
    fn plan_mode_state_reads_the_latest_entry() {
        let dir =
            std::env::temp_dir().join(format!("piwaku-plan-mode-latest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("session.jsonl");
        std::fs::write(
            &file,
            [
                r#"{"type":"custom","customType":"plan-mode-state","data":{"enabled":true}}"#,
                r#"{"type":"assistant","text":"done"}"#,
                r#"{"type":"custom","customType":"plan-mode-state","data":{"enabled":false}}"#,
            ]
            .join("\n"),
        )
        .unwrap();

        assert_eq!(
            read_plan_mode_from_session_file(&file),
            Some(InteractionMode::Build)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_mode_state_absence_defaults_to_build() {
        let dir =
            std::env::temp_dir().join(format!("piwaku-plan-mode-absent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("session.jsonl");
        std::fs::write(&file, r#"{"type":"user","text":"hello"}"#).unwrap();

        assert_eq!(
            read_plan_mode_from_session_file(&file),
            Some(InteractionMode::Build)
        );
        let missing = dir.join("missing.jsonl");
        assert_eq!(read_plan_mode_from_session_file(&missing), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_mode_state_ignores_malformed_entries() {
        let dir =
            std::env::temp_dir().join(format!("piwaku-plan-mode-malformed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("session.jsonl");
        std::fs::write(
            &file,
            [
                r#"{"type":"custom","customType":"plan-mode-state","data":{"enabled":true}}"#,
                r#"{"type":"custom","customType":"plan-mode-state","data":{"enabled":"yes"}}"#,
            ]
            .join("\n"),
        )
        .unwrap();

        assert_eq!(
            read_plan_mode_from_session_file(&file),
            Some(InteractionMode::Build)
        );

        std::fs::write(&file, "{not json\n").unwrap();
        assert_eq!(
            read_plan_mode_from_session_file(&file),
            Some(InteractionMode::Build)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_mode_state_requires_the_custom_entry_type() {
        let dir =
            std::env::temp_dir().join(format!("piwaku-plan-mode-type-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("session.jsonl");
        std::fs::write(
            &file,
            [
                r#"{"type":"custom","customType":"plan-mode-state","data":{"enabled":true}}"#,
                r#"{"customType":"plan-mode-state","data":{"enabled":false}}"#,
                r#"{"type":"assistant","customType":"plan-mode-state","data":{"enabled":false}}"#,
            ]
            .join("\n"),
        )
        .unwrap();

        assert_eq!(
            read_plan_mode_from_session_file(&file),
            Some(InteractionMode::Plan)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn latest_successful_todo_session_entry_hydrates_a_snapshot() {
        let response = serde_json::json!({
            "data": {
                "entries": [
                    {
                        "type": "message",
                        "id": "tool-1",
                        "parentId": null,
                        "message": {
                            "role": "toolResult",
                            "toolName": "todo",
                            "isError": false,
                            "details": {
                                "nextId": 2,
                                "tasks": [{"id": 1, "subject": "old", "status": "completed"}]
                            }
                        }
                    },
                    {
                        "type": "message",
                        "id": "assistant-1",
                        "parentId": "tool-1",
                        "message": {"role": "assistant", "content": []}
                    },
                    {
                        "type": "message",
                        "id": "tool-2",
                        "parentId": "assistant-1",
                        "message": {
                            "role": "toolResult",
                            "toolName": "todo",
                            "isError": false,
                            "details": {
                                "nextId": 3,
                                "tasks": [{"id": 2, "subject": "latest", "status": "in_progress"}]
                            }
                        }
                    }
                ],
                "leafId": "tool-2"
            }
        });

        let snapshot = parse_latest_todo_snapshot_from_entries(&response)
            .expect("latest successful todo entry");
        assert_eq!(snapshot.next_id, 3);
        assert_eq!(snapshot.tasks.len(), 1);
        assert_eq!(snapshot.tasks[0].id, 2);
        assert_eq!(snapshot.tasks[0].subject, "latest");
        assert_eq!(snapshot.tasks[0].status, TodoTaskStatus::InProgress);

        let failed_latest = serde_json::json!({
            "data": {
                "entries": [
                    response["data"]["entries"][0].clone(),
                    {
                        "type": "message",
                        "id": "tool-failed",
                        "parentId": "tool-1",
                        "message": {
                            "role": "toolResult",
                            "toolName": "todo",
                            "isError": true,
                            "details": {
                                "nextId": 9,
                                "tasks": [{"id": 8, "subject": "failed", "status": "pending"}]
                            }
                        }
                    }
                ],
                "leafId": "tool-failed"
            }
        });
        let snapshot = parse_latest_todo_snapshot_from_entries(&failed_latest)
            .expect("failed latest result should not hide prior success");
        assert_eq!(snapshot.tasks[0].subject, "old");

        // The append-only session contains a later result on another branch;
        // leafId/parentId must keep it out of the active-branch hydration.
        let interleaved = serde_json::json!({
            "data": {
                "entries": [
                    {"type": "custom", "id": "root", "parentId": null},
                    {
                        "type": "message",
                        "id": "current-todo",
                        "parentId": "root",
                        "message": {
                            "role": "toolResult",
                            "toolName": "todo",
                            "isError": false,
                            "details": {
                                "nextId": 4,
                                "tasks": [{"id": 3, "subject": "current", "status": "pending"}]
                            }
                        }
                    },
                    {"type": "message", "id": "current-leaf", "parentId": "current-todo"},
                    {
                        "type": "message",
                        "id": "other-branch-todo",
                        "parentId": "root",
                        "message": {
                            "role": "toolResult",
                            "toolName": "todo",
                            "isError": false,
                            "details": {
                                "nextId": 8,
                                "tasks": [{"id": 7, "subject": "wrong branch", "status": "completed"}]
                            }
                        }
                    }
                ],
                "leafId": "current-leaf"
            }
        });
        let snapshot = parse_latest_todo_snapshot_from_entries(&interleaved)
            .expect("active branch todo entry");
        assert_eq!(snapshot.tasks[0].subject, "current");

        assert_eq!(
            parse_latest_todo_snapshot_from_entries(&serde_json::json!({
                "data": {"entries": [], "leafId": null}
            })),
            None
        );
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
    fn permission_select_request_is_strictly_normalized() {
        let parsed = parse_permission_request(&json!({
            "type": "extension_ui_request",
            "id": "permission-1",
            "method": "select",
            "title": "Permission Required\ntool : bash\nvalue : git status",
            "options": [
                "Yes",
                "Yes, allow bash \"git status\" for this session",
                "No",
                "No, provide reason"
            ]
        }))
        .expect("permission select parses");
        assert_eq!(parsed.id, "permission-1");
        assert_eq!(parsed.title, "Permission Required");
        assert_eq!(parsed.detail, "tool : bash\nvalue : git status");
        assert!(parsed.pending.permission);
        assert_eq!(
            parsed
                .options
                .iter()
                .map(|option| (option.id.as_str(), option.label.as_str(), option.allow))
                .collect::<Vec<_>>(),
            vec![
                ("Yes", "Yes", true),
                (
                    "Yes, allow bash \"git status\" for this session",
                    "Yes, allow bash \"git status\" for this session",
                    true
                ),
                ("No", "No", false),
                ("No, provide reason", "No, provide reason", false),
            ]
        );
    }

    #[test]
    fn subagent_permission_heading_is_supported_but_other_selects_are_not() {
        let subagent = json!({
            "type": "extension_ui_request",
            "id": "permission-subagent",
            "method": "select",
            "title": "Permission Required (Subagent)\ncommand : npm test",
            "options": ["Yes", "session grant", "No", "No, provide reason"]
        });
        assert_eq!(
            parse_permission_request(&subagent)
                .expect("subagent permission select parses")
                .title,
            "Permission Required (Subagent)"
        );

        for invalid in [
            json!({
                "method": "confirm",
                "id": "p",
                "title": "Permission Required",
                "options": ["Yes", "session grant", "No", "No, provide reason"]
            }),
            json!({
                "method": "select",
                "id": "p",
                "title": "Permission Requiredly\ncommand : npm test",
                "options": ["Yes", "session grant", "No", "No, provide reason"]
            }),
            json!({
                "method": "select",
                "id": "p",
                "title": " Permission Required\ncommand : npm test",
                "options": ["Yes", "session grant", "No", "No, provide reason"]
            }),
            json!({
                "method": "select",
                "id": "p",
                "title": "Permission Required\ncommand : npm test",
                "options": ["Yes", "session grant", "No"]
            }),
            json!({
                "method": "select",
                "id": "p",
                "title": "Permission Required\ncommand : npm test",
                "options": ["Yes", "", "No", "No, provide reason"]
            }),
            json!({
                "method": "select",
                "id": "p",
                "title": "Permission Required",
                "options": ["Yes", "session grant", "No", "No, provide reason"]
            }),
        ] {
            assert!(
                parse_permission_request(&invalid).is_none(),
                "unrecognized shape must stay out of Permission"
            );
        }

        // A normal select keeps its existing structured-question path.
        let normal = json!({
            "method": "select",
            "id": "select-1",
            "title": "Pick one",
            "options": ["Yes", "session grant", "No", "No, provide reason"]
        });
        assert!(parse_permission_request(&normal).is_none());
        assert!(parse_extension_ui_request(&normal).is_some());
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
    fn pi_fire_and_forget_requests_are_strict_and_bounded() {
        assert_eq!(
            parse_pi_ui_request(&json!({
                "method": "notify",
                "message": "Context refreshed",
                "notifyType": "warning"
            })),
            Some(ParsedPiUiRequest::Notify {
                message: "Context refreshed".into(),
                level: "warning".into(),
            })
        );
        assert_eq!(
            parse_pi_ui_request(&json!({
                "method": "setStatus",
                "statusKey": "magic-context",
                "statusText": "12k / 32k"
            })),
            Some(ParsedPiUiRequest::Status {
                text: Some("12k / 32k".into())
            })
        );
        assert_eq!(
            parse_pi_ui_request(&json!({
                "method": "setStatus",
                "statusKey": "magic-context"
            })),
            Some(ParsedPiUiRequest::Status { text: None })
        );
        for invalid in [
            json!({ "method": "notify", "message": "", "notifyType": "info" }),
            json!({ "method": "notify", "message": "x", "notifyType": "fatal" }),
            json!({ "method": "setStatus", "statusKey": "other", "statusText": "x" }),
            json!({ "method": "setStatus", "statusKey": "magic-context", "statusText": 1 }),
        ] {
            assert!(parse_pi_ui_request(&invalid).is_none());
        }
        let too_long = "x".repeat(MAGIC_STATUS_MAX_CHARS + 1);
        assert!(
            parse_pi_ui_request(&json!({
                "method": "notify",
                "message": too_long,
                "notifyType": "info"
            }))
            .is_none()
        );
    }

    #[test]
    fn magic_status_accepts_bounded_multiline_text_but_not_controls() {
        let entry = |text: &str| {
            json!({
                "type": "custom",
                "customType": "ctx-status",
                "data": {"title": "Magic", "text": text, "level": "info"}
            })
        };
        assert_eq!(
            parse_magic_status_entry(&entry("line one\r\nline\t two")),
            Some(MagicContextStatus {
                title: "Magic".into(),
                text: "line one\nline\t two".into(),
                level: "info".into(),
            })
        );
        assert!(parse_magic_status_entry(&entry("line\u{0000}two")).is_none());
        assert!(
            parse_magic_status_entry(&json!({
                "type": "custom",
                "customType": "ctx-status",
                "data": {"title": "Magic\nstatus", "text": "valid", "level": "info"}
            }))
            .is_none()
        );
        let within_limit = "x".repeat(MAGIC_STATUS_MAX_CHARS + 1);
        assert!(parse_magic_status_entry(&entry(&within_limit)).is_some());
        let over_limit = "x".repeat(MAGIC_PERSISTED_STATUS_MAX_CHARS + 1);
        assert!(parse_magic_status_entry(&entry(&over_limit)).is_none());
    }

    #[test]
    fn magic_session_status_follows_the_active_branch() {
        let dir = std::env::temp_dir().join(format!("piwaku-magic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("session.jsonl");
        std::fs::write(
            &file,
            [
                json!({"type":"session","id":"root","parentId":null}),
                json!({"type":"custom","id":"old","parentId":"root","customType":"ctx-status","data":{"title":"Magic","text":"old","level":"info","details":{"secret":"ignored"}}}),
                json!({"type":"message","id":"branch-a","parentId":"old"}),
                json!({"type":"custom","id":"abandoned","parentId":"branch-a","customType":"ctx-status","data":{"title":"Magic","text":"abandoned","level":"warning"}}),
                json!({"type":"custom","id":"live","parentId":"old","customType":"ctx-status","data":{"title":"Magic","text":"live\r\nstatus","level":"success"}}),
            ]
            .into_iter()
            .map(|entry| serde_json::to_string(&entry).unwrap())
            .collect::<Vec<_>>()
            .join("\n"),
        )
        .unwrap();
        // The final line is the session's current leaf and selects the live
        // branch; a later line on an abandoned branch must not win.
        assert_eq!(
            read_magic_status_from_session_file(&file),
            Some(MagicContextStatus {
                title: "Magic".into(),
                text: "live\nstatus".into(),
                level: "success".into(),
            })
        );
    }

    #[test]
    fn magic_status_reads_a_bounded_tail_of_a_large_session() {
        let dir = std::env::temp_dir().join(format!("piwaku-magic-large-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("session.jsonl");
        let tail = [
            json!({"type":"session","id":"root","parentId":null}),
            json!({"type":"custom","id":"live","parentId":"root","customType":"ctx-status","data":{"title":"Magic","text":"tail status","level":"info"}}),
        ]
        .into_iter()
        .map(|entry| serde_json::to_string(&entry).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
        let prefix = "x".repeat(MAGIC_SESSION_MAX_BYTES as usize + 4096);
        std::fs::write(&file, format!("{prefix}\n{tail}")).unwrap();

        assert_eq!(
            read_magic_status_from_session_file(&file),
            Some(MagicContextStatus {
                title: "Magic".into(),
                text: "tail status".into(),
                level: "info".into(),
            })
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn select_response_echoes_label_and_cancel_maps_to_cancelled() {
        let pending = PendingExtensionUi {
            method: ExtensionUiMethod::Select,
            option_labels: vec!["1. Alpha — a".into(), "2. Beta — b".into()],
            permission: false,
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
            permission: false,
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
            permission: false,
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

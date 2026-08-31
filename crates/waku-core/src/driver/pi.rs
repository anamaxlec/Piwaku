//! Pi RPC transport, shared by Pi and Oh My Pi.
//!
//! Oh My Pi is a fork of Pi that kept the newline-delimited RPC transport but
//! renamed part of the surface: forking is `branch`, a run settles on
//! `agent_end` instead of `agent_settled`, and oversized frames are chunked
//! once protocol v2 is negotiated. [`PiFlavor`] carries those differences so
//! both providers share one transport instead of two near-copies.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use crossbeam_channel::{Sender, bounded, unbounded};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::{activity, computer_use as computer_use_runtime, pi_extensions, tool_progress};
use crate::driver::{
    DriverControl, DriverEventSender, DriverEventSink, DriverStartOptions, SessionOptions,
};
use crate::model::{
    ActivityKind, DriverEvent, GoalOperation, InteractionMode, ProviderResumeCursor, RuntimeMode,
    UserInputAnswer,
};

const RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// Oh My Pi has to start a whole second agent to clone a session, so it needs
/// more headroom than a request against the already-running process.
const CLONE_TIMEOUT: Duration = Duration::from_secs(30);

/// Oh My Pi refuses to reassemble beyond this, so neither should Waku.
const MAX_REASSEMBLED_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Which dialect of the Pi RPC protocol a session speaks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PiFlavor {
    Pi,
    OhMyPi,
}

impl PiFlavor {
    fn display_name(self) -> &'static str {
        match self {
            Self::Pi => "Pi",
            Self::OhMyPi => "Oh My Pi",
        }
    }

    /// Pi has no permission system and only needs project-local files trusted;
    /// Oh My Pi does have one, and Waku only ever runs these in Full access.
    fn full_access_arg(self) -> &'static str {
        match self {
            Self::Pi => "--approve",
            Self::OhMyPi => "--yolo",
        }
    }

    /// The event that means the run is over and no more work is scheduled.
    fn settled_event(self) -> &'static str {
        match self {
            Self::Pi => "agent_settled",
            Self::OhMyPi => "agent_end",
        }
    }

    fn session_info_event(self) -> &'static str {
        match self {
            Self::Pi => "session_info_changed",
            Self::OhMyPi => "session_info_update",
        }
    }

    fn session_info_title_field(self) -> &'static str {
        match self {
            Self::Pi => "name",
            Self::OhMyPi => "title",
        }
    }

    fn branch_messages_command(self) -> &'static str {
        match self {
            Self::Pi => "get_fork_messages",
            Self::OhMyPi => "get_branch_messages",
        }
    }

    fn branch_command(self) -> &'static str {
        match self {
            Self::Pi => "fork",
            Self::OhMyPi => "branch",
        }
    }

    /// Oh My Pi dropped Pi's opt-out env var; it gates its update check on a
    /// setting instead, and does it off the startup path either way.
    fn skips_version_check_by_env(self) -> bool {
        matches!(self, Self::Pi)
    }

    /// Only Oh My Pi chunks oversized frames, and only after it is asked to.
    fn negotiates_protocol_v2(self) -> bool {
        matches!(self, Self::OhMyPi)
    }

    /// Waku's computer-use bridge is a Pi extension written against Pi's
    /// extension API. Oh My Pi ships its own `/computer` instead.
    fn supports_waku_computer_use(self) -> bool {
        matches!(self, Self::Pi)
    }

    fn supports_interaction_mode(self, mode: InteractionMode) -> bool {
        matches!(self, Self::Pi) || mode == InteractionMode::Build
    }

    fn cursor(self, session_id: String, session_file: Option<PathBuf>) -> ProviderResumeCursor {
        match self {
            Self::Pi => ProviderResumeCursor::Pi {
                session_id,
                session_file,
            },
            Self::OhMyPi => ProviderResumeCursor::OhMyPi {
                session_id,
                session_file,
            },
        }
    }

    fn session_file_from_cursor(self, cursor: &ProviderResumeCursor) -> Option<&PathBuf> {
        match (self, cursor) {
            (Self::Pi, ProviderResumeCursor::Pi { session_file, .. })
            | (Self::OhMyPi, ProviderResumeCursor::OhMyPi { session_file, .. }) => {
                session_file.as_ref()
            }
            _ => None,
        }
    }

    fn owns_cursor(self, cursor: &ProviderResumeCursor) -> bool {
        matches!(
            (self, cursor),
            (Self::Pi, ProviderResumeCursor::Pi { .. })
                | (Self::OhMyPi, ProviderResumeCursor::OhMyPi { .. })
        )
    }
}

enum CommandMessage {
    Prompt(String),
    Steer(String),
    Cancel,
    CancelExtensionRequest(String),
    /// PIWAKU: a goal mutation for the pi-goal plugin (Refresh/Clear/Set).
    Goal(GoalOperation),
    /// A fully-built `extension_ui_response` frame for a dialog the native
    /// question panel already answered. Prebuilt so the writer thread stays
    /// free of answer-interpretation logic.
    RespondExtensionUi {
        payload: Value,
    },
    Options(SessionOptions),
    Rollback {
        turns: usize,
        response: Sender<Result<ProviderResumeCursor, String>>,
    },
    Fork {
        turns_to_remove: usize,
        response: Sender<Result<ProviderResumeCursor, String>>,
    },
    Shutdown,
}

type PendingResponses = Arc<Mutex<HashMap<String, Sender<Result<Value, String>>>>>;

/// Extension dialog requests awaiting an answer from the native question
/// panel. The reader thread inserts on `extension_ui_request`; answering
/// (or turn teardown) removes. Shared between reader and command threads.
type PendingExtensionUiRequests = Arc<Mutex<HashMap<String, pi_extensions::PendingExtensionUi>>>;

/// Non-JSON lines observed on the provider's stdout. Extensions run in-process
/// and share its stdout, so any stray `console.log` surfaces here; captured and
/// counted instead of surfaced per line.
#[derive(Default)]
struct StdoutChatter {
    count: u64,
    last: Option<String>,
}

impl StdoutChatter {
    fn record(&mut self, line: &str) {
        self.count += 1;
        let truncated: String = line.trim().chars().take(200).collect();
        self.last = Some(truncated);
    }
}

pub struct PiDriver {
    flavor: PiFlavor,
    commands: Sender<CommandMessage>,
    pending_extension_ui: PendingExtensionUiRequests,
    computer_use: Option<computer_use_runtime::ComputerUseRuntime>,
}

fn configure_pi_computer_use_command(
    command: &mut std::process::Command,
    config: Option<(&computer_use_runtime::ComputerUseConfig, &Path)>,
) {
    if let Some((config, extension)) = config {
        command
            .arg("--extension")
            .arg(extension)
            .arg("--skill")
            .arg(&config.skill_path)
            .env("WAKU_JS_REPL_SERVER", &config.repl_path)
            .env("WAKU_COMPUTER_USE_SERVER", &config.server_path)
            .env(
                "WAKU_COMPUTER_USE_PROCESS_DIRECTORY",
                &config.process_directory,
            );
    }
}

impl PiDriver {
    pub fn start(
        flavor: PiFlavor,
        options: DriverStartOptions,
        events: DriverEventSender,
    ) -> anyhow::Result<Self> {
        let DriverStartOptions {
            binary,
            cwd,
            mode,
            interaction_mode,
            model,
            reasoning_effort,
            service_tier: _,
            context_window: _,
            agent_preset: _,
            computer_use_enabled,
            provider_cursor,
        } = options;
        if mode != RuntimeMode::FullAccess || !flavor.supports_interaction_mode(interaction_mode) {
            return Err(anyhow!(
                "{} currently supports only its native interaction modes with Full access",
                flavor.display_name()
            ));
        }
        let resume_session_file = match provider_cursor {
            Some(cursor) if flavor.owns_cursor(&cursor) => {
                let Some(session_file) = flavor.session_file_from_cursor(&cursor).cloned() else {
                    return Err(anyhow!(
                        "cannot resume {} because its native session file is missing",
                        flavor.display_name()
                    ));
                };
                Some(session_file)
            }
            Some(cursor) => {
                return Err(anyhow!(
                    "cannot resume {} from a {} cursor",
                    flavor.display_name(),
                    cursor.provider().display_name()
                ));
            }
            None => None,
        };
        let new_session = resume_session_file.is_none();
        if let Some(model) = model.as_deref() {
            parse_model_slug(model)?;
        }

        let computer_use = (computer_use_enabled && flavor.supports_waku_computer_use())
            .then(|| computer_use_runtime::ComputerUseRuntime::start(events.clone()))
            .transpose()?;
        let pi_extension = computer_use
            .as_ref()
            .map(|_| crate::computer_use::pi_extension_path())
            .transpose()?;
        let mut command = crate::command_env::command(&binary);
        command.args(["--mode", "rpc", flavor.full_access_arg()]);
        if flavor.skips_version_check_by_env() {
            command.env("PI_SKIP_VERSION_CHECK", "1");
        }
        configure_pi_computer_use_command(
            &mut command,
            computer_use
                .as_ref()
                .zip(pi_extension.as_deref())
                .map(|(runtime, extension)| (&runtime.config, extension)),
        );
        let command = command
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = crate::command_env::spawn(command)
            .with_context(|| format!("failed to start `{} --mode rpc`", binary.display()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("{} stdin unavailable", flavor.display_name()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("{} stdout unavailable", flavor.display_name()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("{} stderr unavailable", flavor.display_name()))?;

        let (commands, command_rx) = unbounded();
        let pending_extension_ui: PendingExtensionUiRequests = Arc::new(Mutex::new(HashMap::new()));
        let waiter_extension_ui = pending_extension_ui.clone();
        let stdout_chatter = Arc::new(Mutex::new(StdoutChatter::default()));
        let reader_chatter = stdout_chatter.clone();
        let waiter_chatter = stdout_chatter.clone();
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let driver_session_state_file: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
        let reader_pending = pending.clone();
        let reader_extension_ui = pending_extension_ui.clone();
        let reader_commands = commands.clone();
        let reader_events = events.clone();
        let reader_session_state_file = driver_session_state_file.clone();
        let reader_thread =
            thread::Builder::new()
                .name("waku-pi-reader".into())
                .spawn(move || {
                    let mut stream_state = PiStreamState::default();
                    let mut chunks = ChunkAssembly::default();
                    for line in BufReader::new(stdout).lines() {
                        match line {
                            Ok(line) if !line.trim().is_empty() => {
                                match serde_json::from_str::<Value>(&line) {
                                    Ok(value) => {
                                        // A chunked frame arrives as an
                                        // uninterrupted run of `rpc_chunk`
                                        // envelopes that reassemble into one
                                        // logical message.
                                        match chunks.accept(value) {
                                            Ok(Some(value)) => handle_pi_message(
                                                flavor,
                                                value,
                                                &reader_pending,
                                                &reader_extension_ui,
                                                &reader_commands,
                                                &reader_events,
                                                &mut stream_state,
                                                &reader_session_state_file,
                                            ),
                                            Ok(None) => {}
                                            Err(error) => {
                                                let _ =
                                                    reader_events.send(DriverEvent::Error(tr!(
                                                        "errors.provider_transport_read",
                                                        provider = flavor.display_name(),
                                                        error = error
                                                    )));
                                            }
                                        }
                                    }
                                    Err(_) => {
                                        // PIWAKU: extensions share the
                                        // process's stdout, so one stray
                                        // `console.log` anywhere lands on the
                                        // RPC stream. That is chatter, not a
                                        // protocol failure — it never breaks
                                        // the turn — so announce it once with
                                        // the offending snippet instead of
                                        // spamming an error per line.
                                        let mut chatter = reader_chatter.lock();
                                        chatter.record(&line);
                                        if chatter.count == 1 {
                                            let snippet = chatter.last.clone().unwrap_or_default();
                                            let _ = reader_events.send(DriverEvent::Error(tr!(
                                                "errors.provider_stdout_noise",
                                                provider = flavor.display_name(),
                                                snippet = snippet
                                            )));
                                        }
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(error) => {
                                let _ = reader_events.send(DriverEvent::Error(tr!(
                                    "errors.provider_transport_read",
                                    provider = flavor.display_name(),
                                    error = error
                                )));
                                break;
                            }
                        }
                    }
                    // Unblock anything waiting on an RPC reply immediately; the
                    // process thread owns the `ProcessExited` announcement so a
                    // non-zero exit can be reported before the runtime is torn down.
                    fail_pending(
                        &reader_pending,
                        &format!("{} RPC process exited", flavor.display_name()),
                    );
                })?;

        let writer_pending = pending;
        let writer_events = events.clone();
        let writer_session_state_file = driver_session_state_file.clone();

        thread::Builder::new()
            .name("waku-pi-writer".into())
            .spawn(move || {
                let mut stdin = stdin;
                let mut next_request_id = 0_u64;
                let initialize = (|| -> Result<Value, String> {
                    // Negotiate before anything else so a large first response
                    // arrives chunked rather than shrunk to an error frame.
                    if flavor.negotiates_protocol_v2() {
                        send_request(
                            &mut stdin,
                            &writer_pending,
                            &mut next_request_id,
                            json!({"type": "negotiate_protocol", "protocolVersion": 2}),
                        )?;
                    }
                    let _ = send_request(
                        &mut stdin,
                        &writer_pending,
                        &mut next_request_id,
                        json!({"type": "get_state"}),
                    )?;
                    if let Some(session_file) = resume_session_file {
                        let response = send_request(
                            &mut stdin,
                            &writer_pending,
                            &mut next_request_id,
                            json!({
                                "type": "switch_session",
                                "sessionPath": session_file
                            }),
                        )?;
                        if response.pointer("/data/cancelled").and_then(Value::as_bool)
                            == Some(true)
                        {
                            return Err(format!(
                                "{} session switch was cancelled",
                                flavor.display_name()
                            ));
                        }
                    }
                    if let Some(model) = model.as_deref() {
                        let (provider, model_id) =
                            parse_model_slug(model).map_err(|error| error.to_string())?;
                        let _ = send_request(
                            &mut stdin,
                            &writer_pending,
                            &mut next_request_id,
                            json!({
                                "type": "set_model",
                                "provider": provider,
                                "modelId": model_id
                            }),
                        )?;
                    }
                    if let Some(level) = reasoning_effort.as_deref() {
                        let _ = send_request(
                            &mut stdin,
                            &writer_pending,
                            &mut next_request_id,
                            json!({"type": "set_thinking_level", "level": level}),
                        )?;
                    }
                    send_request(
                        &mut stdin,
                        &writer_pending,
                        &mut next_request_id,
                        json!({"type": "get_state"}),
                    )
                })();

                let state = match initialize {
                    Ok(state) => state,
                    Err(error) => {
                        let _ = writer_events.send(DriverEvent::Error(tr!(
                            "errors.initialize_provider",
                            provider = flavor.display_name(),
                            error = error
                        )));
                        let _ = writer_events.send(DriverEvent::TurnFinished {
                            success: false,
                            summary: Some(tr!(
                                "errors.provider_initialize_session",
                                provider = flavor.display_name()
                            )),
                        });
                        return;
                    }
                };
                let Some(mut cursor) = cursor_from_state(flavor, &state) else {
                    let _ = writer_events.send(DriverEvent::Error(tr!(
                        "errors.provider_no_session_id",
                        provider = flavor.display_name()
                    )));
                    let _ = writer_events.send(DriverEvent::TurnFinished {
                        success: false,
                        summary: Some(tr!(
                            "errors.provider_initialize_session",
                            provider = flavor.display_name()
                        )),
                    });
                    return;
                };
                let plan_command_error = if flavor == PiFlavor::Pi {
                    match send_request(
                        &mut stdin,
                        &writer_pending,
                        &mut next_request_id,
                        json!({"type": "get_commands"}),
                    ) {
                        Ok(response) if pi_has_plan_command(&response) => None,
                        Ok(_) => Some("Pi /plan command is unavailable; staying in Build mode".to_owned()),
                        Err(error) => Some(format!(
                            "Pi /plan command availability check failed ({error}); staying in Build mode"
                        )),
                    }
                } else {
                    None
                };
                let mut current_interaction_mode = if new_session {
                    InteractionMode::Build
                } else {
                    interaction_mode
                };
                let initial_usage = send_request(
                    &mut stdin,
                    &writer_pending,
                    &mut next_request_id,
                    json!({"type": "get_session_stats"}),
                )
                .ok()
                .and_then(|stats| pi_context_usage(&state, Some(&stats)))
                .or_else(|| pi_context_usage(&state, None));
                // PIWAKU: hydrate the native todo panel from the active Pi
                // branch. A missing/failed read is deliberately non-fatal;
                // new sessions simply have no persisted snapshot yet.
                if flavor == PiFlavor::Pi
                    && let Ok(entries) = send_request(
                        &mut stdin,
                        &writer_pending,
                        &mut next_request_id,
                        json!({"type": "get_entries"}),
                    )
                    && let Some(snapshot) =
                        pi_extensions::parse_latest_todo_snapshot_from_entries(&entries)
                {
                    let _ = writer_events.send(DriverEvent::TodoStateUpdated(snapshot));
                }
                // PIWAKU: track the session file so provider extensions can
                // publish their persisted custom state (resume path included).
                if let ProviderResumeCursor::Pi { session_file, .. } = &cursor {
                    *writer_session_state_file.lock() = session_file.clone();
                    if let Some(session_file) = session_file {
                        if let Some(goal) = pi_extensions::read_goal_from_session_file(session_file)
                        {
                            let _ = writer_events.send(DriverEvent::GoalUpdated(Some(goal)));
                        }
                    }
                    if !new_session && let Some(error) = &plan_command_error {
                        let _ = writer_events.send(DriverEvent::Error(error.clone()));
                        current_interaction_mode = InteractionMode::Build;
                        let _ = writer_events
                            .send(DriverEvent::InteractionModeUpdated(InteractionMode::Build));
                    } else if let Some(session_file) = session_file
                        && let Some(mode) =
                            pi_extensions::read_plan_mode_from_session_file(session_file)
                    {
                        current_interaction_mode = mode;
                        let _ = writer_events.send(DriverEvent::InteractionModeUpdated(mode));
                    }
                    let _ = writer_events.send(DriverEvent::MagicContextStatusUpdated(
                        session_file
                            .as_deref()
                            .and_then(pi_extensions::read_magic_status_from_session_file),
                    ));
                }
                if new_session
                    && flavor == PiFlavor::Pi
                    && interaction_mode == InteractionMode::Plan
                {
                    if let Some(error) = &plan_command_error {
                        let _ = writer_events.send(DriverEvent::Error(error.clone()));
                        let _ = writer_events
                            .send(DriverEvent::InteractionModeUpdated(InteractionMode::Build));
                        let _ = writer_events.send(DriverEvent::TurnFinished {
                            success: false,
                            summary: Some(tr!(
                                "errors.provider_initialize_session",
                                provider = flavor.display_name()
                            )),
                        });
                        return;
                    }
                    if let Err(error) = send_request(
                        &mut stdin,
                        &writer_pending,
                        &mut next_request_id,
                        json!({
                            "type": "prompt",
                            "message": pi_plan_mode_command(InteractionMode::Plan)
                        }),
                    ) {
                        let _ = writer_events.send(DriverEvent::Error(tr!(
                            "errors.provider_rejected_prompt_detail",
                            provider = flavor.display_name(),
                            error = error
                        )));
                        let _ = writer_events
                            .send(DriverEvent::InteractionModeUpdated(InteractionMode::Build));
                        let _ = writer_events.send(DriverEvent::TurnFinished {
                            success: false,
                            summary: Some(tr!(
                                "errors.provider_rejected_prompt",
                                provider = flavor.display_name()
                            )),
                        });
                        return;
                    }
                    if let Some(session_file) = writer_session_state_file.lock().clone() {
                        if let Some(mode) = emit_pi_interaction_mode(&writer_events, &session_file)
                        {
                            current_interaction_mode = mode;
                        } else {
                            let _ = writer_events.send(DriverEvent::InteractionModeUpdated(
                                current_interaction_mode,
                            ));
                        }
                        let _ = writer_events.send(DriverEvent::MagicContextStatusUpdated(
                            pi_extensions::read_magic_status_from_session_file(&session_file),
                        ));
                    } else {
                        let _ = writer_events.send(DriverEvent::InteractionModeUpdated(
                            current_interaction_mode,
                        ));
                    }
                }
                let _ = writer_events.send(DriverEvent::Connected {
                    provider_cursor: Some(cursor.clone()),
                });
                if let Some((context_tokens, context_window)) = initial_usage {
                    let _ = writer_events.send(DriverEvent::UsageUpdated {
                        context_tokens,
                        context_window,
                    });
                }
                if let Some(title) = state
                    .pointer("/data/sessionName")
                    .and_then(Value::as_str)
                    .filter(|title| !title.trim().is_empty())
                {
                    let _ =
                        writer_events.send(DriverEvent::AutoTitleUpdated(Some(title.to_owned())));
                }

                // Both flavors expose setters for these, so changing either is
                // an RPC on the live session rather than a restart.
                let mut current_model = model;
                let mut current_effort = reasoning_effort;
                while let Ok(message) = command_rx.recv() {
                    match message {
                        CommandMessage::Prompt(prompt) => {
                            let result = send_request(
                                &mut stdin,
                                &writer_pending,
                                &mut next_request_id,
                                json!({"type": "prompt", "message": prompt}),
                            );
                            match result {
                                Ok(_) => {
                                    if flavor == PiFlavor::Pi
                                        && let Some(session_file) =
                                            writer_session_state_file.lock().clone()
                                    {
                                        if let Some(mode) =
                                            emit_pi_interaction_mode(&writer_events, &session_file)
                                        {
                                            current_interaction_mode = mode;
                                        }
                                        let _ = writer_events.send(
                                            DriverEvent::MagicContextStatusUpdated(
                                                pi_extensions::read_magic_status_from_session_file(
                                                    &session_file,
                                                ),
                                            ),
                                        );
                                    }
                                }
                                Err(error) => {
                                    let _ = writer_events.send(DriverEvent::Error(tr!(
                                        "errors.provider_rejected_prompt_detail",
                                        provider = flavor.display_name(),
                                        error = error
                                    )));
                                    let _ = writer_events.send(DriverEvent::TurnFinished {
                                        success: false,
                                        summary: Some(tr!(
                                            "errors.provider_rejected_prompt",
                                            provider = flavor.display_name()
                                        )),
                                    });
                                }
                            }
                        }
                        CommandMessage::Steer(prompt) => {
                            let result = send_request(
                                &mut stdin,
                                &writer_pending,
                                &mut next_request_id,
                                json!({"type": "steer", "message": prompt}),
                            );
                            match result {
                                Ok(_) => {
                                    let _ = writer_events
                                        .send(DriverEvent::SteerAccepted { message: prompt });
                                }
                                Err(error) => {
                                    let _ = writer_events.send(DriverEvent::SteerRejected {
                                        message: prompt,
                                        reason: error,
                                    });
                                }
                            }
                        }
                        CommandMessage::Cancel => {
                            if let Err(error) = send_request(
                                &mut stdin,
                                &writer_pending,
                                &mut next_request_id,
                                json!({"type": "abort"}),
                            ) {
                                let _ = writer_events.send(DriverEvent::Error(tr!(
                                    "errors.stop_provider",
                                    provider = flavor.display_name(),
                                    error = error
                                )));
                            }
                        }
                        CommandMessage::Goal(operation) => match operation {
                            GoalOperation::Refresh => {
                                // The daemon asks to re-read state; the
                                // driver owns no goal cache of its own.
                            }
                            GoalOperation::Clear => {
                                let _ = send_request(
                                    &mut stdin,
                                    &writer_pending,
                                    &mut next_request_id,
                                    json!({"type": "prompt", "message": "/goal clear"}),
                                );
                            }
                            GoalOperation::Set {
                                objective, status, ..
                            } => {
                                let message =
                                    pi_extensions::goal_prompt_for_operation(&GoalOperation::Set {
                                        objective: objective.clone(),
                                        status,
                                        replace: false,
                                    });
                                if let Some(message) = message {
                                    let _ = send_request(
                                        &mut stdin,
                                        &writer_pending,
                                        &mut next_request_id,
                                        json!({"type": "prompt", "message": message}),
                                    );
                                }
                            }
                        },
                        CommandMessage::Options(options) => {
                            if options.model != current_model {
                                match options.model.as_deref().map(parse_model_slug).transpose() {
                                    Ok(Some((provider, model_id))) => {
                                        match send_request(
                                            &mut stdin,
                                            &writer_pending,
                                            &mut next_request_id,
                                            json!({
                                                "type": "set_model",
                                                "provider": provider,
                                                "modelId": model_id
                                            }),
                                        ) {
                                            Ok(response) => {
                                                if let Some(window) = response
                                                    .pointer("/data/contextWindow")
                                                    .and_then(Value::as_u64)
                                                    .filter(|window| *window > 0)
                                                {
                                                    let _ = writer_events.send(
                                                        DriverEvent::UsageUpdated {
                                                            context_tokens: None,
                                                            context_window: Some(window),
                                                        },
                                                    );
                                                }
                                            }
                                            Err(error) => {
                                                let _ =
                                                    writer_events.send(DriverEvent::Error(tr!(
                                                        "errors.switch_provider_model",
                                                        provider = flavor.display_name(),
                                                        error = error
                                                    )));
                                            }
                                        }
                                    }
                                    Ok(None) => {}
                                    Err(error) => {
                                        let _ = writer_events
                                            .send(DriverEvent::Error(error.to_string()));
                                    }
                                }
                                current_model = options.model;
                            }
                            if options.reasoning_effort != current_effort {
                                if let Some(level) = options.reasoning_effort.as_deref()
                                    && let Err(error) = send_request(
                                        &mut stdin,
                                        &writer_pending,
                                        &mut next_request_id,
                                        json!({"type": "set_thinking_level", "level": level}),
                                    )
                                {
                                    let _ = writer_events.send(DriverEvent::Error(tr!(
                                        "errors.change_provider_thinking",
                                        provider = flavor.display_name(),
                                        error = error
                                    )));
                                }
                                current_effort = options.reasoning_effort;
                            }
                            if flavor == PiFlavor::Pi {
                                // A user-entered `/plan` command or a plugin
                                // change may have updated the session since
                                // the last Options message. Read the
                                // provider's state before deciding whether a
                                // transition is needed; this is not a second
                                // persisted cache.
                                if let Some(session_file) = writer_session_state_file.lock().clone()
                                    && let Some(mode) =
                                        pi_extensions::read_plan_mode_from_session_file(
                                            &session_file,
                                        )
                                {
                                    current_interaction_mode = mode;
                                }
                            }
                            if flavor == PiFlavor::Pi
                                && options.interaction_mode != current_interaction_mode
                            {
                                let requested_mode = options.interaction_mode;
                                if let Some(error) = &plan_command_error {
                                    let _ = writer_events.send(DriverEvent::Error(error.clone()));
                                    current_interaction_mode = InteractionMode::Build;
                                    let _ = writer_events.send(DriverEvent::InteractionModeUpdated(
                                        InteractionMode::Build,
                                    ));
                                    continue;
                                }
                                match send_request(
                                    &mut stdin,
                                    &writer_pending,
                                    &mut next_request_id,
                                    json!({
                                        "type": "prompt",
                                        "message": pi_plan_mode_command(requested_mode)
                                    }),
                                ) {
                                    Ok(_) => {
                                        if let Some(session_file) =
                                            writer_session_state_file.lock().clone()
                                        {
                                            if let Some(mode) = emit_pi_interaction_mode(
                                                &writer_events,
                                                &session_file,
                                            ) {
                                                current_interaction_mode = mode;
                                            } else {
                                                let _ = writer_events.send(
                                                    DriverEvent::InteractionModeUpdated(
                                                        current_interaction_mode,
                                                    ),
                                                );
                                            }
                                        } else {
                                            let _ = writer_events.send(
                                                DriverEvent::InteractionModeUpdated(
                                                    current_interaction_mode,
                                                ),
                                            );
                                        }
                                    }
                                    Err(error) => {
                                        let _ = writer_events.send(DriverEvent::Error(tr!(
                                            "errors.provider_rejected_prompt_detail",
                                            provider = flavor.display_name(),
                                            error = error
                                        )));
                                        let _ = writer_events.send(
                                            DriverEvent::InteractionModeUpdated(
                                                current_interaction_mode,
                                            ),
                                        );
                                    }
                                }
                            }
                        }
                        CommandMessage::CancelExtensionRequest(id) => {
                            if write_json_line(
                                &mut stdin,
                                &json!({
                                    "type": "extension_ui_response",
                                    "id": id,
                                    "cancelled": true
                                }),
                            )
                            .is_err()
                            {
                                break;
                            }
                        }
                        CommandMessage::RespondExtensionUi { payload } => {
                            if write_json_line(&mut stdin, &payload).is_err() {
                                break;
                            }
                        }
                        CommandMessage::Rollback { turns, response } => {
                            let result = fork_pi_session(
                                flavor,
                                &mut stdin,
                                &writer_pending,
                                &mut next_request_id,
                                &binary,
                                &cwd,
                                &cursor,
                                turns,
                                false,
                            );
                            if let Ok(next_cursor) = &result {
                                cursor = next_cursor.clone();
                            }
                            let _ = response.send(result);
                        }
                        CommandMessage::Fork {
                            turns_to_remove,
                            response,
                        } => {
                            let result = fork_pi_session(
                                flavor,
                                &mut stdin,
                                &writer_pending,
                                &mut next_request_id,
                                &binary,
                                &cwd,
                                &cursor,
                                turns_to_remove,
                                true,
                            );
                            let _ = response.send(result);
                        }
                        CommandMessage::Shutdown => break,
                    }
                }
            })?;

        let last_visible_stderr = Arc::new(Mutex::new(None::<String>));
        let stderr_last_error = last_visible_stderr.clone();
        let stderr_events = events.clone();
        let stderr_thread =
            thread::Builder::new()
                .name("waku-pi-stderr".into())
                .spawn(move || {
                    for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                        if line.to_ascii_lowercase().contains("error") {
                            let error = format!("{}: {}", flavor.display_name(), line.trim());
                            *stderr_last_error.lock() = Some(error.clone());
                            let _ = stderr_events.send(DriverEvent::Error(error));
                        }
                    }
                })?;

        // Nothing signals or kills the agent process: it exits when the writer
        // thread drops its stdin. Something still has to reap it, or every
        // session that ever ran leaves a zombie behind for the life of the app.
        thread::Builder::new()
            .name("waku-pi-process".into())
            .spawn(move || {
                let status = child.wait();
                let _ = reader_thread.join();
                let _ = stderr_thread.join();
                // Any dialog still pending can never be answered now.
                waiter_extension_ui.lock().clear();
                let chatter_count = waiter_chatter.lock().count;
                match status {
                    Ok(status) if !status.success() && last_visible_stderr.lock().is_none() => {
                        let mut message = tr!(
                            "errors.provider_rpc_exited",
                            provider = flavor.display_name(),
                            status = status
                        );
                        if chatter_count > 0 {
                            // PIWAKU: unclean exits are easier to diagnose
                            // when the captured stdout chatter is mentioned.
                            message.push_str(&format!(
                                " (also saw {chatter_count} non-JSON stdout line(s))"
                            ));
                        }
                        let _ = events.send(DriverEvent::Error(message));
                    }
                    Err(error) => {
                        let _ = events.send(DriverEvent::Error(tr!(
                            "errors.read_provider_exit_status",
                            provider = format!("{} RPC", flavor.display_name()),
                            error = error
                        )));
                    }
                    _ => {}
                }
                let _ = events.send(DriverEvent::ProcessExited);
            })?;

        Ok(Self {
            flavor,
            commands,
            pending_extension_ui,
            computer_use,
        })
    }
}

impl DriverControl for PiDriver {
    fn prompt(&self, prompt: String) {
        let _ = self.commands.send(CommandMessage::Prompt(prompt));
    }

    /// PIWAKU: goal state lives in pi-goal's session entries; Refresh
    /// re-reads them, Set/Clear steer the plugin's own `/goal` command so
    /// its runtime (continuation, budgets, safety) stays authoritative.
    fn goal(&self, operation: GoalOperation) {
        let _ = self.commands.send(CommandMessage::Goal(operation));
    }

    fn supports_steer(&self) -> bool {
        true
    }

    fn steer(&self, prompt: String) {
        let _ = self.commands.send(CommandMessage::Steer(prompt));
    }

    fn cancel(&self) {
        let _ = self.commands.send(CommandMessage::Cancel);
    }

    fn cancel_computer_use(&self) {
        if let Some(computer_use) = self.computer_use.as_ref() {
            computer_use.stop();
        }
    }

    /// Answer a permission select from the native permission panel. The
    /// permission extension expects the original label as the select value;
    /// Waku therefore validates the provider option id against the pending
    /// request before reusing the normal extension response builder.
    fn respond(&self, request_id: String, option_id: String) {
        let payload = {
            let mut pending = self.pending_extension_ui.lock();
            let Some(record) = pending.get(&request_id) else {
                return;
            };
            if !record.permission || !record.option_labels.iter().any(|label| label == &option_id) {
                return;
            }
            let record = pending.remove(&request_id).expect("pending record exists");
            record.build_response(
                &request_id,
                &[UserInputAnswer {
                    question_id: request_id.clone(),
                    answers: vec![option_id],
                }],
            )
        };
        let Some(payload) = payload else {
            return;
        };
        let _ = self
            .commands
            .send(CommandMessage::RespondExtensionUi { payload });
    }

    /// Answer an extension dialog from the native question panel. The request
    /// must still be pending; a stale id (already answered, turn settled, or
    /// process gone) is dropped silently — Pi ignores responses for unknown
    /// ids anyway.
    fn respond_user_input(&self, request_id: String, answers: Vec<UserInputAnswer>) {
        let payload = {
            let mut pending = self.pending_extension_ui.lock();
            let Some(record) = pending.remove(&request_id) else {
                return;
            };
            match record.build_response(&request_id, &answers) {
                Some(payload) => payload,
                None => return,
            }
        };
        let _ = self
            .commands
            .send(CommandMessage::RespondExtensionUi { payload });
    }

    /// Decline the dialog: Pi resolves the extension's promise with
    /// `cancelled`, which extensions treat as an explicit user decline (e.g.
    /// ask_user_question's DECLINE envelope) rather than an error.
    fn cancel_user_input(&self, request_id: String) {
        if self
            .pending_extension_ui
            .lock()
            .remove(&request_id)
            .is_none()
        {
            return;
        }
        let _ = self
            .commands
            .send(CommandMessage::CancelExtensionRequest(request_id));
    }

    fn apply_options(&self, options: SessionOptions) -> bool {
        // Both flavors have setters for the model and thinking level, and Pi's
        // plan-mode extension accepts live Build/Plan transitions. Oh My Pi
        // remains Build-only because it does not share that extension.
        if options.mode != RuntimeMode::FullAccess
            || !self
                .flavor
                .supports_interaction_mode(options.interaction_mode)
        {
            return false;
        }
        self.commands.send(CommandMessage::Options(options)).is_ok()
    }

    fn rollback(&self, turns: usize) -> anyhow::Result<Option<ProviderResumeCursor>> {
        if turns == 0 {
            return Ok(None);
        }
        let (response_tx, response_rx) = bounded(1);
        self.commands
            .send(CommandMessage::Rollback {
                turns,
                response: response_tx,
            })
            .with_context(|| {
                format!(
                    "{} driver stopped before rollback",
                    self.flavor.display_name()
                )
            })?;
        response_rx
            .recv_timeout(Duration::from_secs(60))
            .with_context(|| {
                format!(
                    "timed out waiting for {} conversation rollback",
                    self.flavor.display_name()
                )
            })?
            .map(Some)
            .map_err(anyhow::Error::msg)
    }

    fn fork(&self, turns_to_remove: usize) -> anyhow::Result<ProviderResumeCursor> {
        let (response_tx, response_rx) = bounded(1);
        self.commands
            .send(CommandMessage::Fork {
                turns_to_remove,
                response: response_tx,
            })
            .with_context(|| {
                format!(
                    "{} driver stopped before forking",
                    self.flavor.display_name()
                )
            })?;
        response_rx
            .recv_timeout(Duration::from_secs(60))
            .with_context(|| {
                format!(
                    "timed out waiting for {} conversation fork",
                    self.flavor.display_name()
                )
            })?
            .map_err(anyhow::Error::msg)
    }
}

impl Drop for PiDriver {
    fn drop(&mut self) {
        self.cancel_computer_use();
        let _ = self.commands.send(CommandMessage::Shutdown);
    }
}

fn send_request(
    stdin: &mut impl Write,
    pending: &PendingResponses,
    next_request_id: &mut u64,
    mut request: Value,
) -> Result<Value, String> {
    *next_request_id += 1;
    let id = format!("waku-{}", next_request_id);
    request["id"] = Value::String(id.clone());
    let (response_tx, response_rx) = bounded(1);
    pending.lock().insert(id.clone(), response_tx);
    if let Err(error) = write_json_line(stdin, &request) {
        pending.lock().remove(&id);
        return Err(format!("transport write failed: {error}"));
    }
    match response_rx.recv_timeout(RPC_TIMEOUT) {
        Ok(response) => response,
        Err(_) => {
            pending.lock().remove(&id);
            Err(format!(
                "{} timed out",
                request["type"].as_str().unwrap_or("request")
            ))
        }
    }
}

fn write_json_line(writer: &mut impl Write, value: &Value) -> std::io::Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn fail_pending(pending: &PendingResponses, message: &str) {
    for (_, response) in pending.lock().drain() {
        let _ = response.send(Err(message.to_owned()));
    }
}

fn parse_model_slug(model: &str) -> anyhow::Result<(&str, &str)> {
    let Some((provider, model_id)) = model.trim().split_once('/') else {
        return Err(anyhow!(
            "models must use provider/model format; received `{model}`"
        ));
    };
    if provider.is_empty() || model_id.is_empty() {
        return Err(anyhow!(
            "models must use provider/model format; received `{model}`"
        ));
    }
    Ok((provider, model_id))
}

fn cursor_from_state(flavor: PiFlavor, response: &Value) -> Option<ProviderResumeCursor> {
    let session_id = response
        .pointer("/data/sessionId")
        .and_then(Value::as_str)?;
    let session_file = response
        .pointer("/data/sessionFile")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    Some(flavor.cursor(session_id.to_owned(), session_file))
}

fn emit_pi_interaction_mode(
    events: &impl DriverEventSink,
    session_file: &Path,
) -> Option<InteractionMode> {
    let mode = pi_extensions::read_plan_mode_from_session_file(session_file)?;
    let _ = events.send(DriverEvent::InteractionModeUpdated(mode));
    Some(mode)
}

fn pi_has_plan_command(response: &Value) -> bool {
    response
        .pointer("/data/commands")
        .and_then(Value::as_array)
        .is_some_and(|commands| {
            commands.iter().any(|command| {
                command.get("name").and_then(Value::as_str) == Some("plan")
                    && command.get("source").and_then(Value::as_str) == Some("extension")
            })
        })
}

fn pi_plan_mode_command(mode: InteractionMode) -> &'static str {
    match mode {
        InteractionMode::Build => "/plan exit",
        InteractionMode::Plan => "/plan start",
    }
}

/// Reassembles the `rpc_chunk` runs Oh My Pi emits for frames over its 1 MiB
/// stdout ceiling. Without this a large tool result degrades to an error frame
/// and the activity row renders empty.
#[derive(Default)]
struct ChunkAssembly {
    active: Option<PendingChunks>,
}

struct PendingChunks {
    chunk_id: String,
    count: u64,
    next_index: u64,
    byte_length: usize,
    data: Vec<u8>,
}

impl ChunkAssembly {
    /// Returns the logical message to dispatch, or `None` while a chunked
    /// frame is still arriving.
    fn accept(&mut self, value: Value) -> Result<Option<Value>, String> {
        if value.get("type").and_then(Value::as_str) != Some("rpc_chunk") {
            // The run must be uninterrupted, so anything else invalidates a
            // partial frame rather than silently splicing around it.
            if self.active.take().is_some() {
                return Err("chunked frame was interrupted".to_owned());
            }
            return Ok(Some(value));
        }
        let (chunk_id, index, count, byte_length, data) = (|| {
            Some((
                value.get("chunkId").and_then(Value::as_str)?,
                value.get("index").and_then(Value::as_u64)?,
                value.get("count").and_then(Value::as_u64)?,
                value.get("byteLength").and_then(Value::as_u64)?,
                value.get("data").and_then(Value::as_str)?,
            ))
        })()
        .ok_or_else(|| "chunk frame was malformed".to_owned())?;
        let byte_length = usize::try_from(byte_length)
            .map_err(|_| "chunked frame exceeds the reassembly limit".to_owned())?;
        if count == 0 || index >= count {
            self.active = None;
            return Err("chunk frame was malformed".to_owned());
        }
        if byte_length > MAX_REASSEMBLED_FRAME_BYTES {
            self.active = None;
            return Err("chunked frame exceeds the reassembly limit".to_owned());
        }
        let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data)
            .map_err(|error| format!("chunk payload was not valid base64: {error}"))?;

        let pending = match self.active.take() {
            Some(pending)
                if pending.chunk_id == chunk_id
                    && pending.count == count
                    && pending.byte_length == byte_length
                    && pending.next_index == index =>
            {
                pending
            }
            Some(_) => {
                return Err("chunked frame was interrupted".to_owned());
            }
            None if index == 0 => PendingChunks {
                chunk_id: chunk_id.to_owned(),
                count,
                next_index: 0,
                byte_length,
                data: Vec::with_capacity(byte_length),
            },
            None => return Err("chunked frame started mid-sequence".to_owned()),
        };
        let mut pending = pending;
        pending.data.extend_from_slice(&decoded);
        pending.next_index += 1;
        if pending.data.len() > pending.byte_length {
            return Err("chunked frame overran its declared length".to_owned());
        }
        if pending.next_index < pending.count {
            self.active = Some(pending);
            return Ok(None);
        }
        if pending.data.len() != pending.byte_length {
            return Err("chunked frame did not match its declared length".to_owned());
        }
        let text = String::from_utf8(pending.data)
            .map_err(|_| "chunked frame was not valid UTF-8".to_owned())?;
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| format!("chunked frame was not valid JSON: {error}"))
    }
}

/// Pi already computes context occupancy for its own footer. Prefer that
/// native value when session stats are available, and use the active model in
/// `get_state` for the window before the first assistant message arrives.
fn pi_context_usage(state: &Value, stats: Option<&Value>) -> Option<(Option<u64>, Option<u64>)> {
    let context = stats.and_then(|stats| stats.pointer("/data/contextUsage"));
    let tokens = context
        .and_then(|context| context.get("tokens"))
        .and_then(Value::as_u64);
    let window = context
        .and_then(|context| context.get("contextWindow"))
        .and_then(Value::as_u64)
        .or_else(|| {
            state
                .pointer("/data/model/contextWindow")
                .and_then(Value::as_u64)
        })
        .filter(|window| *window > 0);
    (tokens.is_some() || window.is_some()).then_some((tokens, window))
}

/// Pi's providers normally fill `totalTokens`, but Pi itself deliberately
/// falls back to the four component counters when a provider leaves it zero.
/// Keep Waku's meter aligned with that provider-native calculation.
fn pi_message_context_tokens(message: &Value) -> Option<u64> {
    let usage = message.get("usage")?;
    usage
        .get("totalTokens")
        .and_then(Value::as_u64)
        .filter(|tokens| *tokens > 0)
        .or_else(|| {
            let total = ["input", "output", "cacheRead", "cacheWrite"]
                .into_iter()
                .filter_map(|field| usage.get(field).and_then(Value::as_u64))
                .fold(0_u64, u64::saturating_add);
            (total > 0).then_some(total)
        })
}

#[allow(clippy::too_many_arguments)]
fn fork_pi_session(
    flavor: PiFlavor,
    stdin: &mut impl Write,
    pending: &PendingResponses,
    next_request_id: &mut u64,
    binary: &Path,
    cwd: &Path,
    original_cursor: &ProviderResumeCursor,
    turns_to_remove: usize,
    restore_original: bool,
) -> Result<ProviderResumeCursor, String> {
    let name = flavor.display_name();
    let messages = send_request(
        stdin,
        pending,
        next_request_id,
        json!({"type": flavor.branch_messages_command()}),
    )?;
    let messages = messages
        .pointer("/data/messages")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{name} returned an invalid fork-message list"))?;

    // Keeping every turn is a whole-session copy, which Pi does in place and
    // Oh My Pi only does at launch. The out-of-process copy leaves this
    // session untouched, so it never needs restoring afterwards.
    if turns_to_remove == 0 && flavor == PiFlavor::OhMyPi {
        let session_file = flavor
            .session_file_from_cursor(original_cursor)
            .ok_or_else(|| format!("{name}'s original session file is unavailable"))?;
        return clone_ohmypi_session(binary, cwd, session_file);
    }

    let request = pi_fork_request(flavor, messages, turns_to_remove)?;
    let fork = send_request(stdin, pending, next_request_id, request)?;
    if fork.pointer("/data/cancelled").and_then(Value::as_bool) == Some(true) {
        return Err(format!("{name} session fork was cancelled"));
    }
    let fork_state = send_request(
        stdin,
        pending,
        next_request_id,
        json!({"type": "get_state"}),
    )?;
    let fork_cursor = cursor_from_state(flavor, &fork_state)
        .ok_or_else(|| format!("{name} did not report the forked session cursor"))?;

    if restore_original {
        let session_file = flavor
            .session_file_from_cursor(original_cursor)
            .ok_or_else(|| format!("{name}'s original session file is unavailable"))?;
        let switched = send_request(
            stdin,
            pending,
            next_request_id,
            json!({
                "type": "switch_session",
                "sessionPath": session_file
            }),
        )?;
        if switched.pointer("/data/cancelled").and_then(Value::as_bool) == Some(true) {
            return Err(format!(
                "{name} could not return to the source session after forking"
            ));
        }
        let restored_state = send_request(
            stdin,
            pending,
            next_request_id,
            json!({"type": "get_state"}),
        )?;
        let restored_cursor = cursor_from_state(flavor, &restored_state)
            .ok_or_else(|| format!("{name} did not report the restored source session"))?;
        if restored_cursor.native_id() != original_cursor.native_id() {
            return Err(format!(
                "{name} returned to the wrong source session after forking"
            ));
        }
    }

    Ok(fork_cursor)
}

fn pi_fork_request(
    flavor: PiFlavor,
    messages: &[Value],
    turns_to_remove: usize,
) -> Result<Value, String> {
    let name = flavor.display_name();
    if turns_to_remove > messages.len() {
        return Err(format!(
            "{name} has only {} native turns, but Waku needs to remove {turns_to_remove}",
            messages.len()
        ));
    }
    let retained_turns = messages.len() - turns_to_remove;
    if turns_to_remove == 0 {
        // Only reachable for Pi; Oh My Pi copies out of process instead.
        Ok(json!({"type": "clone"}))
    } else {
        let entry_id = messages
            .get(retained_turns)
            .and_then(|message| message.get("entryId"))
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{name} returned a fork message without an entry ID"))?;
        Ok(json!({"type": flavor.branch_command(), "entryId": entry_id}))
    }
}

/// Copies a whole Oh My Pi session by launching a throwaway agent with
/// `--fork`, which is the only place it exposes a full-session copy, then
/// reading back the session the copy landed in.
fn clone_ohmypi_session(
    binary: &Path,
    cwd: &Path,
    session_file: &Path,
) -> Result<ProviderResumeCursor, String> {
    let mut command = crate::command_env::command(binary);
    let command = command
        .args(["--mode", "rpc"])
        .arg(PiFlavor::OhMyPi.full_access_arg())
        .arg("--fork")
        .arg(session_file)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = crate::command_env::spawn(command)
        .map_err(|error| format!("could not start Oh My Pi to copy the session: {error}"))?;
    let result = (|| {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Oh My Pi stdin unavailable".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Oh My Pi stdout unavailable".to_owned())?;
        let (tx, rx) = bounded(1);
        thread::Builder::new()
            .name("waku-ohmypi-clone".into())
            .spawn(move || {
                let mut chunks = ChunkAssembly::default();
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    let Ok(value) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    let Ok(Some(value)) = chunks.accept(value) else {
                        continue;
                    };
                    if value.get("id").and_then(Value::as_str) == Some("waku-clone") {
                        let _ = tx.send(value);
                        break;
                    }
                }
            })
            .map_err(|error| format!("could not read the Oh My Pi session copy: {error}"))?;
        write_json_line(
            &mut stdin,
            &json!({"id": "waku-clone", "type": "get_state"}),
        )
        .map_err(|error| format!("could not ask Oh My Pi for the copied session: {error}"))?;
        let state = rx
            .recv_timeout(CLONE_TIMEOUT)
            .map_err(|_| "timed out waiting for Oh My Pi to copy the session".to_owned())?;
        if state.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(state
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Oh My Pi could not copy the session")
                .to_owned());
        }
        cursor_from_state(PiFlavor::OhMyPi, &state)
            .ok_or_else(|| "Oh My Pi did not report the copied session cursor".to_owned())
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

#[derive(Default)]
struct PiStreamState {
    run_started: bool,
    message_saw_text: bool,
    message_saw_reasoning: bool,
    failed: bool,
    tools: HashMap<String, (ActivityKind, String)>,
}

fn handle_pi_message(
    flavor: PiFlavor,
    value: Value,
    pending: &PendingResponses,
    pending_extension_ui: &PendingExtensionUiRequests,
    // Unused since extension dialogs stopped being auto-cancelled here; the
    // ask_user_question interception adapter will send through it again.
    _commands: &Sender<CommandMessage>,
    events: &impl DriverEventSink,
    state: &mut PiStreamState,
    session_state_file: &Mutex<Option<PathBuf>>,
) {
    let event_type = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if event_type == "response" {
        let Some(id) = value.get("id").and_then(Value::as_str) else {
            return;
        };
        let Some(response) = pending.lock().remove(id) else {
            return;
        };
        if value.get("success").and_then(Value::as_bool) == Some(true) {
            let _ = response.send(Ok(value));
        } else {
            let error = value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{} RPC command failed", flavor.display_name()));
            let _ = response.send(Err(error));
        }
        return;
    }

    if event_type == flavor.session_info_event() {
        let title = value
            .get(flavor.session_info_title_field())
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(str::to_owned);
        let _ = events.send(DriverEvent::AutoTitleUpdated(title));
        return;
    }

    // Oh My Pi reuses `agent_end` for intermediate settles, flagging the real
    // one with `isTerminal`. Anything else here would end the turn early while
    // maintenance or async delivery still has work queued.
    if event_type == flavor.settled_event() {
        if value.get("isTerminal").and_then(Value::as_bool) == Some(false) {
            return;
        }
        if state.run_started {
            let success = !state.failed;
            let _ = events.send(DriverEvent::TurnFinished {
                success,
                summary: (!success).then(|| {
                    tr!(
                        "errors.provider_complete_turn",
                        provider = flavor.display_name()
                    )
                }),
            });
        }
        // Dialogs never outlive the turn that opened them; Pi aborts their
        // tool calls on settle, so any leftover entry is stale.
        pending_extension_ui.lock().clear();
        // PIWAKU: pi-goal writes its `goal-state` entries during turns (the
        // agent may have run `/goal …` itself); re-read on every settle so
        // the panel tracks the plugin without extra protocol.
        if let Some(session_file) = session_state_file.lock().clone() {
            let refreshed = pi_extensions::read_goal_from_session_file(&session_file);
            let _ = events.send(DriverEvent::GoalUpdated(refreshed));
            if flavor == PiFlavor::Pi {
                let _ = emit_pi_interaction_mode(events, &session_file);
                let _ = events.send(DriverEvent::MagicContextStatusUpdated(
                    pi_extensions::read_magic_status_from_session_file(&session_file),
                ));
            }
        }
        *state = PiStreamState::default();
        return;
    }

    match event_type {
        "agent_start" | "turn_start" => {
            if !state.run_started {
                state.run_started = true;
                state.failed = false;
                let _ = events.send(DriverEvent::TurnStarted);
            }
        }
        "message_start" => {
            if value.pointer("/message/role").and_then(Value::as_str) == Some("assistant") {
                state.message_saw_text = false;
                state.message_saw_reasoning = false;
            }
        }
        "message_update" => {
            let update = value.get("assistantMessageEvent").unwrap_or(&Value::Null);
            match update.get("type").and_then(Value::as_str) {
                Some("text_delta") => {
                    if let Some(delta) = update
                        .get("delta")
                        .and_then(Value::as_str)
                        .filter(|delta| !delta.is_empty())
                    {
                        state.message_saw_text = true;
                        let _ = events.send(DriverEvent::TextDelta(delta.to_owned()));
                    }
                }
                Some("thinking_delta") => {
                    if let Some(delta) = update
                        .get("delta")
                        .and_then(Value::as_str)
                        .filter(|delta| !delta.is_empty())
                    {
                        state.message_saw_reasoning = true;
                        let _ = events.send(DriverEvent::ReasoningDelta(delta.to_owned()));
                    }
                }
                Some("error") => {
                    state.failed = true;
                    let _ = events.send(DriverEvent::Error(pi_error_message(flavor, update)));
                }
                _ => {}
            }
        }
        "message_end" => {
            if value.pointer("/message/role").and_then(Value::as_str) == Some("assistant") {
                // This is the context the next call starts from, not the
                // cumulative billed total for the whole session.
                if let Some(tokens) = value.get("message").and_then(pi_message_context_tokens) {
                    let _ = events.send(DriverEvent::UsageUpdated {
                        context_tokens: Some(tokens),
                        context_window: None,
                    });
                }
                emit_completed_message_fallback(value.get("message"), events, state);
            }
        }
        "tool_execution_start" | "tool_execution_update" | "tool_execution_end" => {
            let id = value
                .get("toolCallId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let tool_name = value.get("toolName").and_then(Value::as_str);
            let (kind, mut title) = id
                .as_ref()
                .and_then(|id| state.tools.get(id))
                .cloned()
                .unwrap_or_else(|| {
                    tool_name
                        .map(|tool_name| (classify_tool(tool_name), tool_title(tool_name)))
                        .unwrap_or_else(|| (ActivityKind::Tool, tr!("activity.tool")))
                });
            if event_type == "tool_execution_start"
                && let Some(input_title) = activity::input_title(value.get("args"))
            {
                title = input_title;
            }
            if event_type == "tool_execution_start"
                && let Some(id) = id.as_ref()
            {
                state.tools.insert(id.clone(), (kind, title.clone()));
            }
            let arguments = (event_type == "tool_execution_start")
                .then(|| value.get("args"))
                .flatten();
            let output = match event_type {
                "tool_execution_update" => value.get("partialResult"),
                "tool_execution_end" => value.get("result"),
                _ => None,
            };
            let complete = event_type == "tool_execution_end";
            let failed = value.get("isError").and_then(Value::as_bool) == Some(true);
            // PIWAKU: rpiv-todo returns the full task list on every call —
            // feed the native task panel.
            if complete
                && !failed
                && tool_name == Some("todo")
                && let Some(snapshot) = output.and_then(pi_extensions::parse_todo_snapshot)
            {
                let _ = events.send(DriverEvent::TodoStateUpdated(snapshot));
            }
            let mut item = activity::tool_activity(
                id.clone(),
                kind,
                title,
                arguments,
                output,
                output,
                failed,
                complete,
            );
            // PIWAKU: structured progress from provider-native update
            // payloads (pi-web-access first); completion clears it so the
            // row converges to its settled appearance.
            if !complete
                && let Some(progress) =
                    tool_progress::extract_progress(tool_name, arguments, output)
            {
                item = item.with_progress(progress);
            }
            // PIWAKU: settled web-access rows carry the TUI's status line
            // ("11 sources", "Title (8529 chars)") so completion is visible.
            if complete
                && !failed
                && let Some(summary) = tool_progress::completion_summary(tool_name, output)
            {
                item.display_description = Some(summary);
            }
            let _ = events.send(DriverEvent::RichActivity(item));
            if complete && let Some(id) = id {
                state.tools.remove(&id);
            }
        }
        "auto_retry_end" => {
            if value.get("success").and_then(Value::as_bool) == Some(true) {
                state.failed = false;
            } else {
                state.failed = true;
                let message = value
                    .get("finalError")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| {
                        format!("{} exhausted its automatic retries", flavor.display_name())
                    });
                let _ = events.send(DriverEvent::Error(message));
            }
        }
        "extension_ui_request" => {
            if flavor == PiFlavor::Pi
                && let Some(parsed) = pi_extensions::parse_pi_ui_request(&value)
            {
                match parsed {
                    pi_extensions::ParsedPiUiRequest::Notify { message, level } => {
                        let _ = events.send(DriverEvent::Notification { message, level });
                    }
                    pi_extensions::ParsedPiUiRequest::Status { text } => {
                        let status = text.map(|text| crate::model::MagicContextStatus {
                            title: "Magic Context".to_owned(),
                            text,
                            level: "info".to_owned(),
                        });
                        let _ = events.send(DriverEvent::MagicContextStatusUpdated(status));
                    }
                }
            } else if let Some(parsed) = pi_extensions::parse_permission_request(&value) {
                // PIWAKU: @gotgenes/pi-permission-system's RPC fallback uses
                // a stable select shape. Surface it as a native permission;
                // the pending record retains the original labels for respond.
                pending_extension_ui
                    .lock()
                    .insert(parsed.id.clone(), parsed.pending);
                let _ = events.send(DriverEvent::Permission {
                    request_id: parsed.id,
                    title: parsed.title,
                    detail: parsed.detail,
                    options: parsed.options,
                });
            } else if let Some(parsed) = pi_extensions::parse_extension_ui_request(&value) {
                // PIWAKU: route other extension dialogs to the native question
                // panel instead of auto-cancelling them. Fire-and-forget and
                // unknown methods return None here and are ignored, which
                // keeps unsupported extensions from crashing the runtime.
                pending_extension_ui
                    .lock()
                    .insert(parsed.id.clone(), parsed.pending);
                let _ = events.send(DriverEvent::UserInputRequested {
                    request_id: parsed.id,
                    questions: parsed.questions,
                });
            }
        }
        "extension_error" => {
            let _ = events.send(DriverEvent::Error(pi_error_message(flavor, &value)));
        }
        _ => {}
    }
}

fn emit_completed_message_fallback(
    message: Option<&Value>,
    events: &impl DriverEventSink,
    state: &mut PiStreamState,
) {
    let Some(content) = message
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
    else {
        return;
    };
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") if !state.message_saw_text => {
                if let Some(text) = block
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    state.message_saw_text = true;
                    let _ = events.send(DriverEvent::TextDelta(text.to_owned()));
                }
            }
            Some("thinking") if !state.message_saw_reasoning => {
                if let Some(thinking) = block
                    .get("thinking")
                    .and_then(Value::as_str)
                    .filter(|thinking| !thinking.is_empty())
                {
                    state.message_saw_reasoning = true;
                    let _ = events.send(DriverEvent::ReasoningDelta(thinking.to_owned()));
                }
            }
            _ => {}
        }
    }
}

fn pi_error_message(flavor: PiFlavor, value: &Value) -> String {
    value
        .get("error")
        .and_then(Value::as_str)
        .or_else(|| value.get("errorMessage").and_then(Value::as_str))
        .or_else(|| value.get("reason").and_then(Value::as_str))
        .map(str::to_owned)
        .unwrap_or_else(|| {
            tr!(
                "errors.provider_reported_error",
                provider = flavor.display_name()
            )
        })
}

fn classify_tool(name: &str) -> ActivityKind {
    ActivityKind::from_tool_name(name)
}

fn tool_title(name: &str) -> String {
    match name.to_ascii_lowercase().as_str() {
        "bash" => tr!("activity.run_command"),
        "edit" => tr!("activity.edit_file"),
        "write" => tr!("activity.write_file"),
        "read" => tr!("activity.read_file"),
        "grep" => tr!("activity.search_files"),
        "find" => tr!("activity.find_files"),
        "ls" => tr!("activity.list_files"),
        _ => name.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::TryRecvError;

    fn harness() -> (
        PendingResponses,
        Sender<CommandMessage>,
        crossbeam_channel::Receiver<CommandMessage>,
        PiStreamState,
    ) {
        let (commands, receiver) = unbounded();
        (
            Arc::new(Mutex::new(HashMap::new())),
            commands,
            receiver,
            PiStreamState::default(),
        )
    }

    /// Fresh extension-dialog registry for tests that don't exercise the
    /// dialog bridge.
    /// Tests run without a Pi session file; the goal hooks become no-ops.
    fn no_goal_file() -> Mutex<Option<PathBuf>> {
        Mutex::new(None)
    }

    fn no_extension_ui() -> PendingExtensionUiRequests {
        Arc::new(Mutex::new(HashMap::new()))
    }

    /// Drives the installed Pi RPC through one real provider turn. Ignored by
    /// default because it needs the CLI, credentials, and network access.
    #[test]
    #[ignore = "requires an installed, authenticated pi"]
    fn pi_context_usage_against_the_real_rpc() {
        let binary = crate::command_env::find_executable("pi").expect("pi is not installed");
        let (events, event_rx) = crate::driver::test_event_channel();
        let driver = PiDriver::start(
            PiFlavor::Pi,
            DriverStartOptions {
                binary,
                cwd: std::env::temp_dir(),
                mode: RuntimeMode::FullAccess,
                interaction_mode: InteractionMode::Build,
                model: None,
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                agent_preset: None,
                computer_use_enabled: false,
                provider_cursor: None,
            },
            events,
        )
        .expect("the Pi RPC session should start");

        let mut connected = false;
        let mut context_tokens = None;
        let mut context_window = None;
        while let Ok(event) = event_rx.recv_timeout(Duration::from_secs(30)) {
            match event {
                DriverEvent::Connected { .. } => {
                    connected = true;
                    break;
                }
                DriverEvent::Error(error) => panic!("Pi failed to initialize: {error}"),
                _ => {}
            }
        }
        assert!(connected, "Pi never reported its native session");

        driver.prompt("Reply with exactly: OK. Do not use any tools.".into());
        let mut finished = false;
        while let Ok(event) = event_rx.recv_timeout(Duration::from_secs(180)) {
            match event {
                DriverEvent::UsageUpdated {
                    context_tokens: tokens,
                    context_window: window,
                } => {
                    context_tokens = tokens.or(context_tokens);
                    context_window = window.or(context_window);
                }
                DriverEvent::TurnFinished { success, .. } => {
                    assert!(success, "Pi should finish the probe turn");
                    finished = true;
                    break;
                }
                DriverEvent::Error(error) => panic!("Pi reported: {error}"),
                _ => {}
            }
        }

        assert!(finished, "Pi never settled the probe turn");
        assert!(context_tokens.is_some_and(|tokens| tokens > 0));
        assert!(context_window.is_some_and(|window| window > 0));
    }

    #[test]
    fn model_thinking_and_pi_mode_changes_reach_the_running_session() {
        let (commands, command_rx) = unbounded();
        let driver = PiDriver {
            flavor: PiFlavor::Pi,
            commands,
            pending_extension_ui: no_extension_ui(),
            computer_use: None,
        };
        let options = |mode, interaction_mode| SessionOptions {
            mode,
            interaction_mode,
            model: Some("anthropic/claude-opus-5".to_owned()),
            reasoning_effort: Some("high".to_owned()),
            service_tier: None,
            context_window: None,
        };

        assert!(driver.apply_options(options(RuntimeMode::FullAccess, InteractionMode::Build)));
        assert!(matches!(
            command_rx.try_recv(),
            Ok(CommandMessage::Options(_))
        ));

        assert!(driver.apply_options(options(RuntimeMode::FullAccess, InteractionMode::Plan)));
        assert!(matches!(
            command_rx.try_recv(),
            Ok(CommandMessage::Options(_))
        ));

        // Pi has no permission setter, and still requires Full access.
        assert!(!driver.apply_options(options(RuntimeMode::Ask, InteractionMode::Build)));
        assert!(command_rx.try_recv().is_err());
    }

    #[test]
    fn interaction_mode_eligibility_is_flavor_specific() {
        assert!(PiFlavor::Pi.supports_interaction_mode(InteractionMode::Build));
        assert!(PiFlavor::Pi.supports_interaction_mode(InteractionMode::Plan));
        assert!(PiFlavor::OhMyPi.supports_interaction_mode(InteractionMode::Build));
        assert!(!PiFlavor::OhMyPi.supports_interaction_mode(InteractionMode::Plan));
    }

    #[test]
    fn pi_plan_mode_commands_match_the_extension() {
        assert_eq!(pi_plan_mode_command(InteractionMode::Plan), "/plan start");
        assert_eq!(pi_plan_mode_command(InteractionMode::Build), "/plan exit");
    }

    #[test]
    fn pi_plan_command_availability_requires_a_registered_plan_name() {
        assert!(pi_has_plan_command(&json!({
            "data": {"commands": [{"name": "plan", "source": "extension"}, {"name": "other"}]}
        })));
        assert!(!pi_has_plan_command(&json!({
            "data": {"commands": [{"name": "plan", "source": "prompt"}]}
        })));
        assert!(!pi_has_plan_command(&json!({
            "data": {"commands": [{"name": "other", "source": "extension"}]}
        })));
        assert!(!pi_has_plan_command(&json!({"data": {"commands": "plan"}})));
    }

    #[test]
    fn pi_computer_use_uses_only_session_scoped_extension_and_skill_arguments() {
        let config = computer_use_runtime::ComputerUseConfig {
            server_path: PathBuf::from("/tmp/Waku Computer Use"),
            repl_path: PathBuf::from("/Applications/Waku.app/Resources/waku_js_repl"),
            skill_path: PathBuf::from("/Applications/Waku.app/Resources/skills/SKILL.md"),
            process_directory: PathBuf::from("/tmp/waku-computer-use/session"),
        };
        let mut command = std::process::Command::new("pi");

        configure_pi_computer_use_command(
            &mut command,
            Some((
                &config,
                Path::new("/Applications/Waku.app/Resources/computer-use/pi-extension.ts"),
            )),
        );

        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments,
            [
                "--extension",
                "/Applications/Waku.app/Resources/computer-use/pi-extension.ts",
                "--skill",
                "/Applications/Waku.app/Resources/skills/SKILL.md",
            ]
        );
        let environment = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<HashMap<_, _>>();
        assert_eq!(
            environment.get("WAKU_JS_REPL_SERVER"),
            Some(&Some(
                "/Applications/Waku.app/Resources/waku_js_repl".into()
            ))
        );
        assert_eq!(
            environment.get("WAKU_COMPUTER_USE_PROCESS_DIRECTORY"),
            Some(&Some("/tmp/waku-computer-use/session".into()))
        );
    }

    #[test]
    fn pi_fork_selects_the_first_removed_user_turn_or_clones_the_tip() {
        let messages = [
            json!({"entryId": "turn-1"}),
            json!({"entryId": "turn-2"}),
            json!({"entryId": "turn-3"}),
        ];
        assert_eq!(
            pi_fork_request(PiFlavor::Pi, &messages, 0).unwrap(),
            json!({"type": "clone"})
        );
        assert_eq!(
            pi_fork_request(PiFlavor::Pi, &messages, 2).unwrap(),
            json!({"type": "fork", "entryId": "turn-2"})
        );
        assert_eq!(
            pi_fork_request(PiFlavor::Pi, &messages, 3).unwrap(),
            json!({"type": "fork", "entryId": "turn-1"})
        );
        assert!(pi_fork_request(PiFlavor::Pi, &messages, 4).is_err());
    }

    #[test]
    fn streams_pi_text_reasoning_tools_and_settles_once() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for value in [
            json!({"type": "agent_start"}),
            json!({"type": "turn_start"}),
            json!({
                "type": "message_update",
                "assistantMessageEvent": {"type": "thinking_delta", "delta": "checking"}
            }),
            json!({
                "type": "tool_execution_start",
                "toolCallId": "tool-1",
                "toolName": "read",
                "args": {"path": "src/main.rs", "title": "Inspect Pi source"}
            }),
            json!({
                "type": "tool_execution_end",
                "toolCallId": "tool-1",
                "toolName": "read",
                "result": {"content": "..."},
                "isError": false
            }),
            json!({
                "type": "message_update",
                "assistantMessageEvent": {"type": "text_delta", "delta": "done"}
            }),
            json!({"type": "agent_end", "willRetry": false}),
            json!({"type": "agent_settled"}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                value,
                &pending,
                &no_extension_ui(),
                &commands,
                &events,
                &mut state,
                &no_goal_file(),
            );
        }

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::ReasoningDelta(value) if value == "checking"
        ));
        let DriverEvent::RichActivity(started) = event_rx.recv().unwrap() else {
            panic!("expected a rich Pi tool activity");
        };
        assert_eq!(started.title, "Inspect Pi source");
        assert_eq!(started.kind, ActivityKind::FileRead);
        assert_eq!(started.display_target.as_deref(), Some("src/main.rs"));
        assert!(
            started
                .arguments
                .as_deref()
                .is_some_and(|arguments| arguments.contains("src/main.rs"))
        );
        assert!(!started.complete);
        let DriverEvent::RichActivity(completed) = event_rx.recv().unwrap() else {
            panic!("expected a completed rich Pi tool activity");
        };
        assert!(
            completed
                .output
                .as_deref()
                .is_some_and(|output| output.contains("..."))
        );
        assert!(completed.complete);
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TextDelta(value) if value == "done"
        ));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn pi_settled_reloads_persisted_plan_mode() {
        let dir =
            std::env::temp_dir().join(format!("piwaku-pi-plan-mode-event-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("session.jsonl");
        std::fs::write(
            &file,
            r#"{"type":"custom","customType":"plan-mode-state","data":{"enabled":true}}"#,
        )
        .unwrap();

        let (pending, commands, _command_rx, mut state) = harness();
        state.run_started = true;
        let (events, event_rx) = unbounded();
        let session_state_file = Mutex::new(Some(file.clone()));
        handle_pi_message(
            PiFlavor::Pi,
            json!({"type": "agent_settled"}),
            &pending,
            &no_extension_ui(),
            &commands,
            &events,
            &mut state,
            &session_state_file,
        );

        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::GoalUpdated(None)
        ));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::InteractionModeUpdated(InteractionMode::Plan)
        ));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::MagicContextStatusUpdated(None)
        ));
        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Drives the installed Oh My Pi RPC through one real provider turn.
    /// Ignored by default because it needs the CLI, credentials, and network.
    #[test]
    #[ignore = "requires an installed, authenticated omp"]
    fn ohmypi_context_usage_against_the_real_rpc() {
        let binary = crate::command_env::find_executable("omp").expect("omp is not installed");
        let (events, event_rx) = crate::driver::test_event_channel();
        let driver = PiDriver::start(
            PiFlavor::OhMyPi,
            DriverStartOptions {
                binary,
                cwd: std::env::temp_dir(),
                mode: RuntimeMode::FullAccess,
                interaction_mode: InteractionMode::Build,
                model: None,
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                agent_preset: None,
                computer_use_enabled: false,
                provider_cursor: None,
            },
            events,
        )
        .expect("the Oh My Pi RPC session should start");

        let mut cursor = None;
        while let Ok(event) = event_rx.recv_timeout(Duration::from_secs(60)) {
            match event {
                DriverEvent::Connected { provider_cursor } => {
                    cursor = provider_cursor;
                    break;
                }
                DriverEvent::Error(error) => panic!("Oh My Pi failed to initialize: {error}"),
                _ => {}
            }
        }
        assert!(
            matches!(
                cursor,
                Some(ProviderResumeCursor::OhMyPi {
                    session_file: Some(_),
                    ..
                })
            ),
            "Oh My Pi should report its own cursor with a session file, got {cursor:?}"
        );

        driver.prompt("Reply with exactly: OK. Do not use any tools.".into());
        let mut finished = false;
        let mut context_tokens = None;
        let mut context_window = None;
        while let Ok(event) = event_rx.recv_timeout(Duration::from_secs(180)) {
            match event {
                DriverEvent::UsageUpdated {
                    context_tokens: tokens,
                    context_window: window,
                } => {
                    context_tokens = tokens.or(context_tokens);
                    context_window = window.or(context_window);
                }
                DriverEvent::TurnFinished { success, .. } => {
                    assert!(success, "Oh My Pi should finish the probe turn");
                    finished = true;
                    break;
                }
                DriverEvent::Error(error) => panic!("Oh My Pi reported: {error}"),
                _ => {}
            }
        }

        assert!(finished, "Oh My Pi never settled the probe turn");
        assert!(context_tokens.is_some_and(|tokens| tokens > 0));
        assert!(context_window.is_some_and(|window| window > 0));
    }

    #[test]
    fn ohmypi_branches_where_pi_forks_and_never_asks_it_to_clone() {
        let messages = [
            json!({"entryId": "turn-1"}),
            json!({"entryId": "turn-2"}),
            json!({"entryId": "turn-3"}),
        ];
        assert_eq!(
            pi_fork_request(PiFlavor::OhMyPi, &messages, 2).unwrap(),
            json!({"type": "branch", "entryId": "turn-2"})
        );
        assert!(pi_fork_request(PiFlavor::OhMyPi, &messages, 4).is_err());
    }

    /// Oh My Pi reuses `agent_end` for intermediate settles, so only the
    /// terminal one may end the turn.
    #[test]
    fn ohmypi_settles_on_the_terminal_agent_end_only() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for value in [
            json!({"type": "agent_start"}),
            json!({
                "type": "message_update",
                "assistantMessageEvent": {"type": "text_delta", "delta": "done"}
            }),
            json!({"type": "agent_end", "isTerminal": false}),
            json!({"type": "agent_end", "messages": []}),
        ] {
            handle_pi_message(
                PiFlavor::OhMyPi,
                value,
                &pending,
                &no_extension_ui(),
                &commands,
                &events,
                &mut state,
                &no_goal_file(),
            );
        }

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TextDelta(value) if value == "done"
        ));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    /// Pi's own settle event carries no meaning for Oh My Pi, and vice versa.
    #[test]
    fn each_flavor_ignores_the_other_settle_and_title_events() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for (flavor, value) in [
            (PiFlavor::OhMyPi, json!({"type": "agent_start"})),
            (PiFlavor::OhMyPi, json!({"type": "agent_settled"})),
            (
                PiFlavor::OhMyPi,
                json!({"type": "session_info_changed", "name": "Pi's spelling"}),
            ),
            (PiFlavor::Pi, json!({"type": "agent_end"})),
            (
                PiFlavor::Pi,
                json!({"type": "session_info_update", "title": "Oh My Pi's spelling"}),
            ),
        ] {
            handle_pi_message(
                flavor,
                value,
                &pending,
                &no_extension_ui(),
                &commands,
                &events,
                &mut state,
                &no_goal_file(),
            );
        }

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn ohmypi_session_titles_arrive_on_its_own_event() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::OhMyPi,
            json!({"type": "session_info_update", "title": "Named by Oh My Pi"}),
            &pending,
            &no_extension_ui(),
            &commands,
            &events,
            &mut state,
            &no_goal_file(),
        );
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::AutoTitleUpdated(Some(title)) if title == "Named by Oh My Pi"
        ));
    }

    #[test]
    fn chunked_frames_reassemble_and_reject_broken_runs() {
        use base64::Engine as _;
        let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);

        let payload = json!({"type": "response", "id": "waku-1", "success": true});
        let bytes = serde_json::to_vec(&payload).unwrap();
        let (first, second) = bytes.split_at(bytes.len() / 2);
        let chunk = |index: u64, data: &[u8]| {
            json!({
                "type": "rpc_chunk",
                "chunkId": "rpc-1",
                "index": index,
                "count": 2,
                "byteLength": bytes.len(),
                "data": encode(data),
            })
        };

        let mut assembly = ChunkAssembly::default();
        assert_eq!(assembly.accept(chunk(0, first)).unwrap(), None);
        assert_eq!(assembly.accept(chunk(1, second)).unwrap(), Some(payload));

        // An ordinary frame passes straight through.
        let mut assembly = ChunkAssembly::default();
        let plain = json!({"type": "agent_start"});
        assert_eq!(assembly.accept(plain.clone()).unwrap(), Some(plain.clone()));

        // Anything interleaved into a run invalidates it rather than splicing.
        let mut assembly = ChunkAssembly::default();
        assert_eq!(assembly.accept(chunk(0, first)).unwrap(), None);
        assert!(assembly.accept(plain).is_err());
        assert!(assembly.active.is_none());

        // A run that starts mid-sequence is not a frame Waku can trust.
        let mut assembly = ChunkAssembly::default();
        assert!(assembly.accept(chunk(1, second)).is_err());
    }

    #[test]
    fn session_name_changes_are_forwarded_as_automatic_titles() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();

        handle_pi_message(
            PiFlavor::Pi,
            json!({"type": "session_info_changed", "name": "Named by Pi"}),
            &pending,
            &no_extension_ui(),
            &commands,
            &events,
            &mut state,
            &no_goal_file(),
        );
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::AutoTitleUpdated(Some(title)) if title == "Named by Pi"
        ));

        handle_pi_message(
            PiFlavor::Pi,
            json!({"type": "session_info_changed", "name": null}),
            &pending,
            &no_extension_ui(),
            &commands,
            &events,
            &mut state,
            &no_goal_file(),
        );
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::AutoTitleUpdated(None)
        ));
    }

    #[test]
    fn tool_only_intermediate_message_does_not_emit_empty_text() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::Pi,
            json!({
                "type": "message_end",
                "message": {
                    "role": "assistant",
                    "content": [{"type": "toolCall", "id": "tool-1", "name": "read"}]
                }
            }),
            &pending,
            &no_extension_ui(),
            &commands,
            &events,
            &mut state,
            &no_goal_file(),
        );

        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn completed_message_is_used_when_deltas_were_not_streamed() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::Pi,
            json!({
                "type": "message_end",
                "message": {
                    "role": "assistant",
                    "content": [
                        {"type": "thinking", "thinking": "reason"},
                        {"type": "text", "text": "answer"}
                    ]
                }
            }),
            &pending,
            &no_extension_ui(),
            &commands,
            &events,
            &mut state,
            &no_goal_file(),
        );

        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::ReasoningDelta(value) if value == "reason"
        ));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TextDelta(value) if value == "answer"
        ));
    }

    #[test]
    fn context_usage_uses_pi_components_when_total_is_zero() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::Pi,
            json!({
                "type": "message_end",
                "message": {
                    "role": "assistant",
                    "content": [],
                    "usage": {
                        "input": 33,
                        "output": 27,
                        "cacheRead": 5888,
                        "cacheWrite": 4,
                        "totalTokens": 0
                    }
                }
            }),
            &pending,
            &no_extension_ui(),
            &commands,
            &events,
            &mut state,
            &no_goal_file(),
        );

        assert!(matches!(
            event_rx.try_recv().unwrap(),
            DriverEvent::UsageUpdated {
                context_tokens: Some(5952),
                context_window: None
            }
        ));
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn session_stats_supply_pi_context_tokens_and_window() {
        let state = json!({"data": {"model": {"contextWindow": 200_000}}});
        let stats = json!({
            "data": {
                "contextUsage": {
                    "tokens": 6109,
                    "contextWindow": 1_000_000,
                    "percent": 0.6109
                }
            }
        });

        assert_eq!(
            pi_context_usage(&state, Some(&stats)),
            Some((Some(6109), Some(1_000_000)))
        );
        assert_eq!(pi_context_usage(&state, None), Some((None, Some(200_000))));
    }

    #[test]
    fn recoverable_tool_error_does_not_fail_the_turn() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for value in [
            json!({"type": "agent_start"}),
            json!({
                "type": "tool_execution_end",
                "toolCallId": "tool-1",
                "toolName": "read",
                "result": {"error": "missing"},
                "isError": true
            }),
            json!({"type": "agent_settled"}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                value,
                &pending,
                &no_extension_ui(),
                &commands,
                &events,
                &mut state,
                &no_goal_file(),
            );
        }

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        let DriverEvent::RichActivity(completed) = event_rx.recv().unwrap() else {
            panic!("expected a completed rich Pi tool activity");
        };
        assert!(completed.failed);
        assert!(
            completed
                .output
                .as_deref()
                .is_some_and(|output| output.contains("missing"))
        );
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
    }

    #[test]
    fn successful_auto_retry_recovers_the_turn() {
        let (pending, commands, _command_rx, mut state) = harness();
        let (events, event_rx) = unbounded();
        for value in [
            json!({"type": "agent_start"}),
            json!({
                "type": "message_update",
                "assistantMessageEvent": {"type": "error", "error": "temporary"}
            }),
            json!({"type": "auto_retry_end", "success": true}),
            json!({"type": "agent_settled"}),
        ] {
            handle_pi_message(
                PiFlavor::Pi,
                value,
                &pending,
                &no_extension_ui(),
                &commands,
                &events,
                &mut state,
                &no_goal_file(),
            );
        }

        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::TurnStarted));
        assert!(matches!(event_rx.recv().unwrap(), DriverEvent::Error(_)));
        assert!(matches!(
            event_rx.recv().unwrap(),
            DriverEvent::TurnFinished { success: true, .. }
        ));
    }

    #[test]
    fn permission_extension_request_uses_native_permission_and_response() {
        let (pending, commands, command_rx, mut state) = harness();
        let pending_extension_ui = no_extension_ui();
        let (events, event_rx) = unbounded();
        handle_pi_message(
            PiFlavor::Pi,
            json!({
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
            }),
            &pending,
            &pending_extension_ui,
            &commands,
            &events,
            &mut state,
            &no_goal_file(),
        );

        let DriverEvent::Permission {
            request_id,
            title,
            detail,
            options,
        } = event_rx.recv().expect("permission event")
        else {
            panic!("permission select must use the native permission event");
        };
        assert_eq!(request_id, "permission-1");
        assert_eq!(title, "Permission Required");
        assert_eq!(detail, "tool : bash\nvalue : git status");
        assert_eq!(options.len(), 4);
        assert!(options[0].allow && options[1].allow);
        assert!(!options[2].allow && !options[3].allow);

        let selected = options[1].id.clone();
        let driver = PiDriver {
            flavor: PiFlavor::Pi,
            commands,
            pending_extension_ui,
            computer_use: None,
        };
        driver.respond("permission-1".into(), selected.clone());
        let Ok(CommandMessage::RespondExtensionUi { payload }) = command_rx.recv() else {
            panic!("permission response must reach the writer");
        };
        assert_eq!(
            payload,
            json!({
                "type": "extension_ui_response",
                "id": "permission-1",
                "value": selected
            })
        );

        // Unknown/stale options never consume or write a response.
        driver.respond("permission-1".into(), "not-an-option".into());
        assert!(command_rx.try_recv().is_err());
    }

    /// PIWAKU: drives the full extension-dialog round trip against a fake RPC
    /// process — request normalization out, panel answer back as the exact
    /// `extension_ui_response` frame. Ignored because it spawns a local
    /// fixture script; run with `cargo test -p waku-core -- --ignored`.
    #[test]
    #[cfg(unix)]
    #[ignore = "spawns a local fake pi process"]
    fn extension_dialog_round_trip_against_a_fake_rpc_process() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = std::env::temp_dir().join("piwaku-fake-pi-test.mjs");
        let session_file = std::path::Path::new("/tmp/fake-pi-session.jsonl");
        let received_log = std::path::Path::new("/tmp/piwaku-fake-received.log");
        let no_plan_flag = std::path::Path::new("/tmp/piwaku-fake-no-plan");
        let todo_flag = std::path::Path::new("/tmp/piwaku-fake-todo");
        let _ = std::fs::remove_file(session_file);
        let _ = std::fs::remove_file(received_log);
        let _ = std::fs::remove_file(no_plan_flag);
        let _ = std::fs::remove_file(todo_flag);
        std::fs::write(
            &fixture,
            r#"#!/usr/bin/env node
import fs from "node:fs";
let buf = "";
let planStarts = 0;
let activeSessionFile = "/tmp/fake-pi-session.jsonl";
function out(o) { process.stdout.write(JSON.stringify(o) + "\n"); }
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buf += chunk;
  let i;
  while ((i = buf.indexOf("\n")) >= 0) {
    const line = buf.slice(0, i).trim();
    buf = buf.slice(i + 1);
    if (!line) continue;
    let m; try { m = JSON.parse(line); } catch { continue; }
    fs.appendFileSync('/tmp/piwaku-fake-received.log', JSON.stringify(m) + '\n');
    if (m.type === "extension_ui_response") {
      const ok = m.value === "1. Alpha \u2014 a";
      process.stderr.write(ok ? "FAKEPI ok\n" : `FAKEPI error mismatch: ${JSON.stringify(m)}\n`);
      out({ type: "agent_start" });
      out({ type: "agent_settled" });
      setTimeout(() => process.exit(ok ? 0 : 2), 150);
    } else if (m.type === "prompt") {
      if (m.message === "/plan start") {
        planStarts += 1;
        // Simulate a successful RPC whose extension state refuses the second
        // transition; Waku must report the persisted Build state, not Plan.
        if (planStarts === 1) {
          fs.writeFileSync('/tmp/fake-pi-session.jsonl', JSON.stringify({type:'custom',customType:'plan-mode-state',data:{enabled:true}}) + '\n');
        }
      } else if (m.message === "/plan exit") {
        fs.writeFileSync('/tmp/fake-pi-session.jsonl', JSON.stringify({type:'custom',customType:'plan-mode-state',data:{enabled:false}}) + '\n');
      } else {
        out({ type: "extension_ui_request", id: "t-1", method: "select", title: "Pick", options: ["1. Alpha \u2014 a", "2. Beta \u2014 b"] });
      }
      out({ type: "response", id: m.id ?? "x", success: true });
    } else if (m.type === "get_commands") {
      const commands = fs.existsSync('/tmp/piwaku-fake-no-plan') ? [] : [{name:'plan',source:'extension'}];
      out({ type: "response", id: m.id ?? "x", success: true, data: { commands } });
    } else if (m.type === "get_entries") {
      const entries = fs.existsSync('/tmp/piwaku-fake-todo') ? [{
        type: 'message',
        id: 'todo-1',
        message: {
          role: 'toolResult',
          toolName: 'todo',
          isError: false,
          details: { nextId: 2, tasks: [{ id: 1, subject: 'resume todo', status: 'pending' }] }
        },
        parentId: null
      }] : [];
      out({ type: "response", id: m.id ?? "x", success: true, data: { entries, leafId: entries.length ? 'todo-1' : null } });
    } else if (m.type === "switch_session") {
      activeSessionFile = m.sessionPath;
      out({ type: "response", id: m.id ?? "x", success: true, data: { sessionFile: activeSessionFile } });
    } else if (m.type && m.type !== "abort") {
      out({ type: "response", id: m.id ?? "x", success: true, data: { model: "test/model", sessionId: "s", sessionFile: activeSessionFile } });
    }
  }
});
"#,
        )
        .expect("write fixture");
        std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fixture");

        // A Plan request must fail before Connected when the command is not
        // registered, and it must never send the raw /plan prompt.
        std::fs::write(no_plan_flag, "").expect("disable fake plan command");
        let (missing_events, missing_rx) = crate::driver::test_event_channel();
        let missing_driver = PiDriver::start(
            PiFlavor::Pi,
            DriverStartOptions {
                binary: fixture.clone(),
                cwd: std::env::temp_dir(),
                mode: RuntimeMode::FullAccess,
                interaction_mode: InteractionMode::Plan,
                model: None,
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                agent_preset: None,
                computer_use_enabled: false,
                provider_cursor: None,
            },
            missing_events,
        )
        .expect("fake Pi without plan command should start its RPC process");
        let mut saw_error = false;
        let mut saw_build = false;
        let mut saw_failed_turn = false;
        let mut saw_connected = false;
        while !(saw_error && saw_build && saw_failed_turn) {
            match missing_rx.recv_timeout(Duration::from_secs(15)) {
                Ok(DriverEvent::Error(_)) => saw_error = true,
                Ok(DriverEvent::InteractionModeUpdated(InteractionMode::Build)) => saw_build = true,
                Ok(DriverEvent::TurnFinished { success: false, .. }) => saw_failed_turn = true,
                Ok(DriverEvent::Connected { .. }) => saw_connected = true,
                Err(_) => panic!("timed out waiting for unavailable plan command"),
                _ => {}
            }
        }
        assert!(!saw_connected);
        drop(missing_driver);
        let missing_prompts = std::fs::read_to_string(received_log)
            .expect("fake RPC should record get_commands")
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|message| message.get("type").and_then(Value::as_str) == Some("prompt"))
            .count();
        assert_eq!(missing_prompts, 0);

        // A resumed Plan session with no registered command must also project
        // Build even when its session file cannot be read.
        let resume_session_file = std::env::temp_dir().join(format!(
            "piwaku-fake-resume-missing-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&resume_session_file);
        std::fs::write(todo_flag, "").expect("enable fake todo hydration");
        let (resume_events, resume_rx) = crate::driver::test_event_channel();
        let resume_driver = PiDriver::start(
            PiFlavor::Pi,
            DriverStartOptions {
                binary: fixture.clone(),
                cwd: std::env::temp_dir(),
                mode: RuntimeMode::FullAccess,
                interaction_mode: InteractionMode::Plan,
                model: None,
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                agent_preset: None,
                computer_use_enabled: false,
                provider_cursor: Some(ProviderResumeCursor::Pi {
                    session_id: "resume".to_owned(),
                    session_file: Some(resume_session_file.clone()),
                }),
            },
            resume_events,
        )
        .expect("fake Pi resume should start its RPC process");
        let mut resume_saw_error = false;
        let mut resume_saw_build = false;
        let mut resume_saw_connected = false;
        let mut resume_saw_todo = false;
        while !(resume_saw_error && resume_saw_build && resume_saw_connected && resume_saw_todo) {
            match resume_rx.recv_timeout(Duration::from_secs(15)) {
                Ok(DriverEvent::Error(_)) => resume_saw_error = true,
                Ok(DriverEvent::InteractionModeUpdated(InteractionMode::Build)) => {
                    resume_saw_build = true
                }
                Ok(DriverEvent::Connected { .. }) => resume_saw_connected = true,
                Ok(DriverEvent::TodoStateUpdated(snapshot)) => {
                    assert_eq!(snapshot.tasks[0].subject, "resume todo");
                    resume_saw_todo = true;
                }
                Err(_) => panic!("timed out waiting for unreadable resume state fallback"),
                _ => {}
            }
        }
        drop(resume_driver);
        assert!(!resume_saw_connected || resume_saw_build);
        let _ = std::fs::remove_file(&resume_session_file);
        let _ = std::fs::remove_file(no_plan_flag);
        let _ = std::fs::remove_file(todo_flag);

        let (events, event_rx) = crate::driver::test_event_channel();
        let driver = PiDriver::start(
            PiFlavor::Pi,
            DriverStartOptions {
                binary: fixture.clone(),
                cwd: std::env::temp_dir(),
                mode: RuntimeMode::FullAccess,
                interaction_mode: InteractionMode::Plan,
                model: None,
                reasoning_effort: None,
                service_tier: None,
                context_window: None,
                agent_preset: None,
                computer_use_enabled: false,
                provider_cursor: None,
            },
            events,
        )
        .expect("fake Pi session should start");

        // Handshake completes on its own; mode commands are tested before the
        // ordinary prompt/extension round trip.
        loop {
            match event_rx.recv_timeout(Duration::from_secs(15)) {
                Ok(DriverEvent::Connected { .. }) => break,
                Ok(DriverEvent::Error(error)) => panic!("fixture failed to initialize: {error}"),
                Err(_) => panic!("timed out waiting for handshake"),
                _ => {}
            }
        }

        assert!(driver.apply_options(SessionOptions {
            mode: RuntimeMode::FullAccess,
            interaction_mode: InteractionMode::Build,
            model: None,
            reasoning_effort: None,
            service_tier: None,
            context_window: None,
        }));
        loop {
            match event_rx.recv_timeout(Duration::from_secs(15)) {
                Ok(DriverEvent::InteractionModeUpdated(InteractionMode::Build)) => break,
                Ok(DriverEvent::Error(error)) => panic!("mode switch failed: {error}"),
                Err(_) => panic!("timed out waiting for Build mode"),
                _ => {}
            }
        }

        assert!(driver.apply_options(SessionOptions {
            mode: RuntimeMode::FullAccess,
            interaction_mode: InteractionMode::Plan,
            model: None,
            reasoning_effort: None,
            service_tier: None,
            context_window: None,
        }));
        loop {
            match event_rx.recv_timeout(Duration::from_secs(15)) {
                // The fake acknowledges /plan start but deliberately leaves
                // persisted state at Build.
                Ok(DriverEvent::InteractionModeUpdated(InteractionMode::Build)) => break,
                Ok(DriverEvent::Error(error)) => panic!("mode switch failed: {error}"),
                Err(_) => panic!("timed out waiting for persisted Build mode"),
                _ => {}
            }
        }

        let received_prompts = std::fs::read_to_string(received_log)
            .expect("fake RPC should record received frames")
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter_map(|message| {
                (message.get("type").and_then(Value::as_str) == Some("prompt"))
                    .then(|| {
                        message
                            .get("message")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .flatten()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            received_prompts,
            ["/plan start", "/plan exit", "/plan start"]
        );

        driver.prompt("go".into());
        let mut answered_request_id = None;
        let mut finished = false;
        while !finished {
            match event_rx.recv_timeout(Duration::from_secs(15)) {
                Ok(DriverEvent::UserInputRequested {
                    request_id,
                    questions,
                }) => {
                    assert_eq!(questions.len(), 1);
                    assert_eq!(questions[0].options.len(), 2);
                    driver.respond_user_input(
                        request_id.clone(),
                        vec![UserInputAnswer {
                            question_id: request_id.clone(),
                            answers: vec!["1. Alpha \u{2014} a".to_owned()],
                        }],
                    );
                    answered_request_id = Some(request_id);
                }
                Ok(DriverEvent::TurnFinished { success: true, .. }) => {
                    finished = true;
                }
                Ok(DriverEvent::Error(error)) => {
                    panic!("dialog round trip surfaced an error: {error}");
                }
                Err(_) => panic!("timed out mid dialog; answered={answered_request_id:?}"),
                _ => {}
            }
        }
        assert_eq!(answered_request_id.as_deref(), Some("t-1"));
        let _ = std::fs::remove_file(&fixture);
        let _ = std::fs::remove_file(session_file);
        let _ = std::fs::remove_file(received_log);
        let _ = std::fs::remove_file(todo_flag);
    }
}

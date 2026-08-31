//! PIWAKU: pi package inventory and enable/disable, daemon-host side.
//!
//! Every path here belongs to the daemon host — the client's own `~/.pi` is
//! irrelevant when talking to a remote daemon. pi's settings `packages`
//! array is the single source of truth for what loads; it has no
//! whole-package disabled flag, so disabling removes the entry and the
//! daemon's own settings remember what to offer re-enabling.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use anyhow::Context as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::{
    PiExtensionInfo, PiExtensionScope, PiExtensionSetting, PiExtensionSettingsGroup,
    PiProjectSettingsSnapshot, PiSettingsScopeSnapshot, PiSettingsSnapshot,
};

/// A pi installation's agent directory (`~/.pi/agent` on the daemon host).
fn agent_dir(home: &Path) -> PathBuf {
    home.join(".pi").join("agent")
}

/// Piwaku's own record of packages disabled through the extensions manager.
/// pi's settings have no whole-package disabled flag, so disabling removes
/// the entry from pi's `packages` array — and without this record the
/// package would simply vanish. Daemon-local on purpose: the record must
/// never ride through the shared DaemonSettings, which the desktop
/// overwrites wholesale on every settings sync.
fn disabled_record_path(home: &Path) -> PathBuf {
    agent_dir(home).join("piwaku-disabled-packages.json")
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct DisabledPackage {
    source: String,
    scope: PiExtensionScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_root: Option<PathBuf>,
    /// The exact settings entry removed on disable. Keeping it here lets an
    /// object-form package regain its filters/autoload settings on enable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entry: Option<Value>,
}

fn load_disabled(home: &Path) -> Vec<DisabledPackage> {
    let Ok(bytes) = fs::read(disabled_record_path(home)) else {
        return Vec::new();
    };
    if let Ok(records) = serde_json::from_slice::<Vec<DisabledPackage>>(&bytes) {
        return records;
    }
    // Before scoped records existed this file was a Vec<String>. Treat those
    // records as user-scope entries and retain the source as the restorable
    // plain settings value.
    serde_json::from_slice::<Vec<String>>(&bytes)
        .unwrap_or_default()
        .into_iter()
        .map(|source| DisabledPackage {
            entry: Some(Value::String(source.clone())),
            source,
            scope: PiExtensionScope::User,
            project_root: None,
        })
        .collect()
}

fn save_disabled(home: &Path, disabled: &[DisabledPackage]) -> anyhow::Result<()> {
    let path = disabled_record_path(home);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, serde_json::to_vec_pretty(disabled)?)?;
    Ok(())
}

fn user_settings_path(home: &Path) -> PathBuf {
    agent_dir(home).join("settings.json")
}

fn project_settings_path(project_root: &Path) -> PathBuf {
    project_root.join(".pi").join("settings.json")
}

fn global_extensions_settings_path(home: &Path) -> PathBuf {
    agent_dir(home).join("settings-extensions.json")
}

fn project_extensions_settings_path(project_root: &Path) -> PathBuf {
    project_root.join(".pi").join("settings-extensions.json")
}

fn read_optional_json(path: &Path) -> Result<Option<Value>, String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("{}: {error}", path.display()))
}

const PI_SETTINGS_MASKED: &str = "[REDACTED]";
const PI_SETTINGS_TRUNCATED: &str = "[TRUNCATED]";
const MAX_EXTENSION_SETTING_STRING_CHARS: usize = 256;
const MAX_EXTENSION_SETTING_NESTED_ITEMS: usize = 32;
const MAX_EXTENSION_SETTING_DEPTH: usize = 4;
const MAX_EXTENSION_SETTINGS_GROUPS: usize = 64;
const MAX_EXTENSION_SETTINGS_ENTRIES: usize = 256;
const MAX_EXTENSION_SETTINGS_ENTRIES_PER_GROUP: usize = 64;
const MAX_EXTENSION_SETTINGS_PAYLOAD_BYTES: usize = 64 * 1024;

fn normalized_setting_key(key: &str) -> String {
    key.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

fn sensitive_setting_key(key: &str) -> bool {
    let normalized = normalized_setting_key(key);
    [
        "token",
        "secret",
        "password",
        "apikey",
        "auth",
        "credential",
        "privatekey",
        "accesskey",
    ]
    .into_iter()
    .any(|needle| normalized.contains(needle))
}

fn truncate_setting_text(value: &str) -> String {
    let mut characters = value.chars();
    let mut truncated = characters
        .by_ref()
        .take(MAX_EXTENSION_SETTING_STRING_CHARS)
        .collect::<String>();
    if characters.next().is_some() {
        truncated.push('…');
    }
    truncated
}

fn sanitize_extension_value(key: &str, value: &Value, depth: usize) -> Value {
    if sensitive_setting_key(key) {
        return Value::String(PI_SETTINGS_MASKED.to_owned());
    }
    match value {
        Value::String(value) => Value::String(truncate_setting_text(value)),
        Value::Array(_) if depth >= MAX_EXTENSION_SETTING_DEPTH => {
            Value::String(PI_SETTINGS_TRUNCATED.to_owned())
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .take(MAX_EXTENSION_SETTING_NESTED_ITEMS)
                .map(|value| sanitize_extension_value("", value, depth + 1))
                .collect(),
        ),
        Value::Object(_) if depth >= MAX_EXTENSION_SETTING_DEPTH => {
            Value::String(PI_SETTINGS_TRUNCATED.to_owned())
        }
        Value::Object(values) => {
            let mut sanitized = serde_json::Map::new();
            for (key, value) in values.iter().take(MAX_EXTENSION_SETTING_NESTED_ITEMS) {
                sanitized.insert(
                    truncate_setting_text(key),
                    sanitize_extension_value(key, value, depth + 1),
                );
            }
            Value::Object(sanitized)
        }
        value => value.clone(),
    }
}

fn sanitize_extension_entries(values: &serde_json::Map<String, Value>) -> Vec<PiExtensionSetting> {
    let mut keys = values.keys().collect::<Vec<_>>();
    keys.sort();
    keys.into_iter()
        .take(MAX_EXTENSION_SETTINGS_ENTRIES_PER_GROUP)
        .map(|key| PiExtensionSetting {
            key: truncate_setting_text(key),
            value: sanitize_extension_value(key, &values[key], 0),
        })
        .collect()
}

fn limit_extension_settings_payload(
    mut groups: Vec<PiExtensionSettingsGroup>,
) -> Vec<PiExtensionSettingsGroup> {
    groups.sort_by(|a, b| a.extension.cmp(&b.extension));
    groups.truncate(MAX_EXTENSION_SETTINGS_GROUPS);

    let mut remaining_entries = MAX_EXTENSION_SETTINGS_ENTRIES;
    for group in &mut groups {
        group
            .entries
            .truncate(remaining_entries.min(MAX_EXTENSION_SETTINGS_ENTRIES_PER_GROUP));
        remaining_entries = remaining_entries.saturating_sub(group.entries.len());
    }

    while serde_json::to_vec(&groups).map_or(true, |payload| {
        payload.len() > MAX_EXTENSION_SETTINGS_PAYLOAD_BYTES
    }) {
        let Some(group) = groups.last_mut() else {
            break;
        };
        if group.entries.pop().is_none() {
            groups.pop();
        }
    }
    groups
}

fn read_pi_settings_scope(
    config_path: PathBuf,
    extensions_path: PathBuf,
) -> PiSettingsScopeSnapshot {
    let mut errors = Vec::new();
    let mut default_provider = None;
    let mut default_model = None;
    let mut default_thinking_level = None;
    let mut quiet_startup = None;

    match read_optional_json(&config_path) {
        Ok(None) => {}
        Ok(Some(Value::Object(settings))) => {
            default_provider = settings
                .get("defaultProvider")
                .and_then(Value::as_str)
                .map(str::to_owned);
            default_model = settings
                .get("defaultModel")
                .and_then(Value::as_str)
                .map(str::to_owned);
            default_thinking_level = settings
                .get("defaultThinkingLevel")
                .and_then(Value::as_str)
                .map(str::to_owned);
            quiet_startup = settings.get("quietStartup").and_then(Value::as_bool);
        }
        Ok(Some(_)) => errors.push(format!("{}: root must be an object", config_path.display())),
        Err(error) => errors.push(error),
    }

    let mut extension_settings = Vec::new();
    match read_optional_json(&extensions_path) {
        Ok(None) => {}
        Ok(Some(Value::Object(settings))) => {
            for (extension, values) in settings {
                let Some(values) = values.as_object() else {
                    errors.push(format!(
                        "{}: extension {extension:?} must be an object",
                        extensions_path.display()
                    ));
                    continue;
                };
                extension_settings.push(PiExtensionSettingsGroup {
                    extension: truncate_setting_text(&extension),
                    entries: sanitize_extension_entries(values),
                });
            }
            extension_settings = limit_extension_settings_payload(extension_settings);
        }
        Ok(Some(_)) => errors.push(format!(
            "{}: root must be an object",
            extensions_path.display()
        )),
        Err(error) => errors.push(error),
    }

    PiSettingsScopeSnapshot {
        config_path,
        extensions_path,
        default_provider,
        default_model,
        default_thinking_level,
        quiet_startup,
        extension_settings,
        error: (!errors.is_empty()).then(|| errors.join("; ")),
    }
}

/// Read stable Pi settings on the daemon host. Missing files are empty scopes;
/// malformed files stay visible as a scope error instead of aborting peers.
pub fn load_settings_snapshot(home: &Path, projects: &[(String, PathBuf)]) -> PiSettingsSnapshot {
    let global = read_pi_settings_scope(
        user_settings_path(home),
        global_extensions_settings_path(home),
    );
    let projects = projects
        .iter()
        .map(|(name, root)| PiProjectSettingsSnapshot {
            name: name.clone(),
            project_root: root.clone(),
            settings: read_pi_settings_scope(
                project_settings_path(root),
                project_extensions_settings_path(root),
            ),
        })
        .collect();
    PiSettingsSnapshot { global, projects }
}

/// Update only Pi's global `quietStartup` key while retaining every other
/// setting verbatim in the parsed JSON object.
pub fn set_quiet_startup(home: &Path, enabled: bool) -> anyhow::Result<()> {
    let path = user_settings_path(home);
    let mut settings = read_optional_json(&path)
        .map_err(|error| anyhow::anyhow!(error))?
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    let object = settings
        .as_object_mut()
        .context("pi settings root is not an object")?;
    object.insert("quietStartup".to_owned(), Value::Bool(enabled));
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, serde_json::to_string_pretty(&settings)?)?;
    Ok(())
}

/// Package name from a settings source string: `npm:@a/b` → `@a/b`,
/// `git:…`/`github:…` keep their last segment, local paths keep their file
/// name.
fn source_name(source: &str) -> String {
    let without_transport = source.split_once(':').map_or(source, |(_, rest)| rest);
    let trimmed = without_transport.trim_end_matches('/');
    Path::new(trimmed)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| trimmed.to_owned())
}

/// The installed package directory for a settings source, under the npm
/// store. Scoped packages keep their `@org/` directory: `npm:@a/b` resolves
/// to `node_modules/@a/b`, not `node_modules/b`.
fn store_dir(home: &Path, source: &str) -> PathBuf {
    let without_transport = source
        .split_once(':')
        .filter(|(transport, _)| matches!(*transport, "npm" | "git" | "github"))
        .map_or(source, |(_, rest)| rest);
    let trimmed = without_transport.trim_end_matches('/');
    let package = if trimmed.starts_with('@') || !trimmed.contains('/') {
        trimmed.to_owned()
    } else {
        // git/github sources install under their package name.
        source_name(source)
    };
    agent_dir(home)
        .join("npm")
        .join("node_modules")
        .join(package)
}

fn entry_source(entry: &Value) -> Option<&str> {
    match entry {
        Value::String(source) => Some(source.as_str()),
        Value::Object(object) => object.get("source").and_then(Value::as_str),
        _ => None,
    }
}

fn entry_filters(entry: &Value) -> Vec<String> {
    match entry {
        Value::Object(object) => object
            .get("extensions")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|entry| entry.starts_with('-'))
                    .map(|entry| entry.trim_start_matches('-').to_owned())
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Read one settings file's `packages` entries; missing file → empty.
fn read_packages(path: &Path) -> Vec<Value> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|settings| settings.get("packages").and_then(Value::as_array).cloned())
        .unwrap_or_default()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ListedPackage {
    source: String,
    scope: PiExtensionScope,
    project_root: Option<PathBuf>,
}

fn same_identity(
    source: &str,
    scope: PiExtensionScope,
    project_root: Option<&Path>,
    other_source: &str,
    other_scope: PiExtensionScope,
    other_project_root: Option<&Path>,
) -> bool {
    source == other_source && scope == other_scope && project_root == other_project_root
}

/// Parse the stable `pi 0.84.3 list` text. The source is the two-space line
/// under a scope heading; the four-space installed path is deliberately not
/// part of the identity because it is daemon-host-specific.
fn parse_pi_list(output: &str) -> Vec<ListedPackage> {
    let mut scope = None;
    let mut packages = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        scope = match trimmed {
            "User packages:" => Some(PiExtensionScope::User),
            "Project packages:" => Some(PiExtensionScope::Project),
            _ => scope,
        };
        let indentation = line.len() - line.trim_start().len();
        if indentation != 2 {
            continue;
        }
        let Some(scope) = scope else { continue };
        let source = trimmed
            .strip_suffix(" (filtered)")
            .unwrap_or(trimmed)
            .trim();
        if source.is_empty()
            || source.starts_with('/')
            || source.ends_with(':')
            || !(source.starts_with("npm:")
                || source.starts_with("extensions/")
                || source.starts_with("git:")
                || source.starts_with("github:")
                || source.starts_with("./")
                || source.starts_with("../"))
        {
            continue;
        }
        if !packages
            .iter()
            .any(|package: &ListedPackage| package.source == source && package.scope == scope)
        {
            packages.push(ListedPackage {
                source: source.to_owned(),
                scope,
                project_root: None,
            });
        }
    }
    packages
}

const PI_LIST_TIMEOUT: Duration = Duration::from_secs(5);
const PI_EXTENSION_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const PI_EXTENSION_MUTATION_TIMEOUT: Duration = Duration::from_secs(60);

/// Run the daemon-host Pi binary in one project context. A failed or hung
/// command returns `None`, so callers can safely retain the settings inventory.
fn list_pi_packages(binary: &Path, cwd: &Path) -> Option<Vec<ListedPackage>> {
    list_pi_packages_with_timeout(binary, cwd, PI_LIST_TIMEOUT)
}

struct CommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
}

fn abort_pi_command_child(
    child: &mut Child,
    stdout_reader: JoinHandle<Option<Vec<u8>>>,
    stderr_reader: JoinHandle<()>,
) {
    #[cfg(unix)]
    {
        // The CLI is often a shell/Node wrapper. Kill its process group so a
        // descendant holding a pipe cannot keep the reader threads alive.
        let process_group = -(child.id() as libc::pid_t);
        let _ = unsafe { libc::kill(process_group, libc::SIGKILL) };
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = stdout_reader.join();
    let _ = stderr_reader.join();
}

fn run_pi_command_with_timeout(
    binary: &Path,
    cwd: &Path,
    args: &[&str],
    timeout: Duration,
) -> Option<CommandOutput> {
    let mut command = crate::command_env::command(binary);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = crate::command_env::spawn(&mut command).ok()?;
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let Some(mut stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).ok().map(|_| bytes)
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
    });
    let deadline = Instant::now() + timeout;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // A shell/Node wrapper may have exited while a descendant
                // still owns one of the captured pipes. Bound that wait too;
                // otherwise a successful parent can hang this read forever.
                while !stdout_reader.is_finished() || !stderr_reader.is_finished() {
                    if Instant::now() >= deadline {
                        abort_pi_command_child(&mut child, stdout_reader, stderr_reader);
                        return None;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                let stdout = stdout_reader.join().ok().flatten()?;
                let _ = stderr_reader.join();
                return Some(CommandOutput { status, stdout });
            }
            Ok(None) if Instant::now() >= deadline => {
                abort_pi_command_child(&mut child, stdout_reader, stderr_reader);
                return None;
            }
            Err(_) => {
                abort_pi_command_child(&mut child, stdout_reader, stderr_reader);
                return None;
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn list_pi_packages_with_timeout(
    binary: &Path,
    cwd: &Path,
    timeout: Duration,
) -> Option<Vec<ListedPackage>> {
    let output = run_pi_command_with_timeout(binary, cwd, &["list", "--approve"], timeout)?;
    output
        .status
        .success()
        .then(|| parse_pi_list(&String::from_utf8_lossy(&output.stdout)))
}

fn extension_command_cwd(
    home: &Path,
    scope: PiExtensionScope,
    project_root: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    match scope {
        PiExtensionScope::User if project_root.is_none() => Ok(home.to_owned()),
        PiExtensionScope::Project => project_root
            .map(Path::to_path_buf)
            .context("project-scope extensions need a project root"),
        PiExtensionScope::User => {
            anyhow::bail!("user-scope extensions cannot have a project root")
        }
    }
}

/// Pi's package commands accept a source as one positional argument. Keep the
/// accepted shapes in sync with the inventory parser and reject flag-like or
/// malformed values before they can reach the provider CLI.
fn valid_extension_source(source: &str) -> bool {
    !source.is_empty()
        && !source.starts_with('-')
        && !source.starts_with('/')
        && !source.ends_with(':')
        && !source
            .chars()
            .any(|character| character.is_whitespace() || character == '\0')
        && (source.starts_with("npm:")
            || source.starts_with("extensions/")
            || source.starts_with("git:")
            || source.starts_with("github:")
            || source.starts_with("./")
            || source.starts_with("../"))
}

fn valid_npm_source(source: &str) -> Option<&str> {
    let package = source.strip_prefix("npm:")?;
    (valid_extension_source(source)
        && !package.is_empty()
        && !package.starts_with('-')
        && !package.starts_with('/')
        && !package
            .chars()
            .any(|character| character.is_whitespace() || character == '\0'))
    .then_some(package)
}

fn valid_mutation_source(source: &str) -> bool {
    valid_extension_source(source)
        && (!source.starts_with("npm:") || valid_npm_source(source).is_some())
}

fn authorize_extension_identity(
    binary: Option<&Path>,
    home: &Path,
    source: &str,
    scope: PiExtensionScope,
    project_root: Option<&Path>,
    projects: &[(String, PathBuf)],
    require_enabled: bool,
) -> anyhow::Result<PathBuf> {
    if !valid_mutation_source(source) {
        anyhow::bail!("unsupported pi extension source");
    }
    let cwd = extension_command_cwd(home, scope, project_root)?;
    if scope == PiExtensionScope::Project && !projects.iter().any(|(_, root)| root == &cwd) {
        anyhow::bail!("project is not registered with the daemon");
    }

    let inventory = load_extensions_with_pi_list(home, projects, binary);
    let Some(identity) = inventory.into_iter().find(|extension| {
        same_identity(
            &extension.source,
            extension.scope,
            extension.project_root.as_deref(),
            source,
            scope,
            project_root,
        )
    }) else {
        anyhow::bail!("pi extension is not an inventory entry");
    };
    if !identity.manageable {
        anyhow::bail!("pi extension is not manageable");
    }
    if require_enabled && !identity.enabled {
        anyhow::bail!("pi extension is not an enabled inventory entry");
    }
    Ok(cwd)
}

/// Validate one extension identity against the same daemon-host inventory used
/// by the extensions page. Set enable/disable accepts either state so a
/// disabled row can be restored; mutation commands require an enabled row.
pub fn validate_extension_identity(
    binary: Option<&Path>,
    home: &Path,
    source: &str,
    scope: PiExtensionScope,
    project_root: Option<&Path>,
    projects: &[(String, PathBuf)],
    require_enabled: bool,
) -> anyhow::Result<()> {
    authorize_extension_identity(
        binary,
        home,
        source,
        scope,
        project_root,
        projects,
        require_enabled,
    )
    .map(|_| ())
}

fn run_extension_command(
    binary: &Path,
    home: &Path,
    source: &str,
    scope: PiExtensionScope,
    project_root: Option<&Path>,
    projects: &[(String, PathBuf)],
    args: &[&str],
) -> anyhow::Result<()> {
    let cwd = authorize_extension_identity(
        Some(binary),
        home,
        source,
        scope,
        project_root,
        projects,
        true,
    )?;
    let output = run_pi_command_with_timeout(binary, &cwd, args, PI_EXTENSION_MUTATION_TIMEOUT)
        .ok_or_else(|| anyhow::anyhow!("pi command timed out or could not start"))?;
    if !output.status.success() {
        anyhow::bail!("pi command failed for {source}");
    }
    Ok(())
}

/// Check one npm package through the daemon host's npm installation. A
/// missing package, malformed output, failed command, or timeout is simply
/// unavailable; callers must not infer an update from any of those states.
pub fn check_extension_update(source: &str, home: &Path) -> Option<String> {
    let package = valid_npm_source(source)?;
    let npm = crate::command_env::find_executable("npm")?;
    check_extension_update_with_npm(&npm, home, package, PI_EXTENSION_CHECK_TIMEOUT)
}

fn check_extension_update_with_npm(
    npm: &Path,
    cwd: &Path,
    package: &str,
    timeout: Duration,
) -> Option<String> {
    let output = run_pi_command_with_timeout(npm, cwd, &["view", package, "version"], timeout)?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let version = stdout.lines().find(|line| !line.trim().is_empty())?.trim();
    (!version.is_empty() && !version.chars().any(char::is_whitespace)).then(|| version.to_owned())
}

/// Update one inventory identity through Pi's official updater.
pub fn update_extension(
    binary: &Path,
    home: &Path,
    source: &str,
    scope: PiExtensionScope,
    project_root: Option<&Path>,
    projects: &[(String, PathBuf)],
) -> anyhow::Result<()> {
    run_extension_command(
        binary,
        home,
        source,
        scope,
        project_root,
        projects,
        &["update", source, "--approve"],
    )
}

/// Remove one inventory identity through Pi's official remover.
pub fn remove_extension(
    binary: &Path,
    home: &Path,
    source: &str,
    scope: PiExtensionScope,
    project_root: Option<&Path>,
    projects: &[(String, PathBuf)],
) -> anyhow::Result<()> {
    let mut args = vec!["remove", source];
    if scope == PiExtensionScope::Project {
        args.push("--local");
    }
    args.push("--approve");
    run_extension_command(binary, home, source, scope, project_root, projects, &args)
}

fn pi_list_inventory(
    binary: Option<&Path>,
    home: &Path,
    projects: &[(String, PathBuf)],
) -> Option<Vec<ListedPackage>> {
    let binary = binary?;
    let mut contexts = Vec::with_capacity(projects.len() + 1);
    contexts.push((home.to_owned(), None));
    contexts.extend(
        projects
            .iter()
            .map(|(_, root)| (root.clone(), Some(root.clone()))),
    );

    let mut listed = Vec::new();
    for (cwd, project_root) in contexts {
        for mut package in list_pi_packages(binary, &cwd)? {
            if package.scope == PiExtensionScope::Project {
                package.project_root = project_root.clone();
            }
            if !listed.iter().any(|existing: &ListedPackage| {
                existing.source == package.source
                    && existing.scope == package.scope
                    && existing.project_root == package.project_root
            }) {
                listed.push(package);
            }
        }
    }
    Some(listed)
}

/// Version and description from the package's installed package.json, best
/// effort — an unreadable store still lists the package, just bare.
fn store_metadata(home: &Path, source: &str) -> (Option<String>, Option<String>) {
    let manifest = store_dir(home, source).join("package.json");
    let Ok(bytes) = fs::read(manifest) else {
        return (None, None);
    };
    let Ok(manifest) = serde_json::from_slice::<Value>(&bytes) else {
        return (None, None);
    };
    (
        manifest
            .get("version")
            .and_then(Value::as_str)
            .map(str::to_owned),
        manifest
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
    )
}

/// Inventory installed pi packages across the global scope and the given
/// project scopes, merged with the daemon's disabled record so disabled
/// packages stay visible and re-enableable.
pub fn load_extensions(home: &Path, projects: &[(String, PathBuf)]) -> Vec<PiExtensionInfo> {
    let disabled = load_disabled(home);
    let mut extensions: Vec<PiExtensionInfo> = Vec::new();
    fn seen(
        extensions: &[PiExtensionInfo],
        source: &str,
        scope: PiExtensionScope,
        project_root: Option<&Path>,
    ) -> bool {
        extensions.iter().any(|extension| {
            same_identity(
                &extension.source,
                extension.scope,
                extension.project_root.as_deref(),
                source,
                scope,
                project_root,
            )
        })
    }

    for entry in read_packages(&user_settings_path(home)) {
        let Some(source) = entry_source(&entry).map(str::to_owned) else {
            continue;
        };
        if seen(&extensions, &source, PiExtensionScope::User, None) {
            continue;
        }
        let (version, description) = store_metadata(home, &source);
        extensions.push(PiExtensionInfo {
            name: source_name(&source),
            version,
            description,
            scope: PiExtensionScope::User,
            enabled: true,
            manageable: true,
            configured: true,
            filtered: entry_filters(&entry),
            source,
            project_root: None,
        });
    }

    for (_, root) in projects {
        for entry in read_packages(&project_settings_path(root)) {
            let Some(source) = entry_source(&entry).map(str::to_owned) else {
                continue;
            };
            if seen(&extensions, &source, PiExtensionScope::Project, Some(root)) {
                continue;
            }
            let (version, description) = store_metadata(home, &source);
            extensions.push(PiExtensionInfo {
                name: source_name(&source),
                version,
                description,
                scope: PiExtensionScope::Project,
                enabled: true,
                manageable: true,
                configured: true,
                filtered: entry_filters(&entry),
                source,
                project_root: Some(root.clone()),
            });
        }
    }

    for record in &disabled {
        if seen(
            &extensions,
            &record.source,
            record.scope,
            record.project_root.as_deref(),
        ) {
            continue;
        }
        // Only offer re-enabling what is still installed.
        let (version, _) = store_metadata(home, &record.source);
        if version.is_none() {
            continue;
        }
        extensions.push(PiExtensionInfo {
            name: source_name(&record.source),
            version,
            description: None,
            scope: record.scope,
            enabled: false,
            manageable: true,
            configured: true,
            filtered: record.entry.as_ref().map(entry_filters).unwrap_or_default(),
            source: record.source.clone(),
            project_root: record.project_root.clone(),
        });
    }

    extensions.sort_by(|a, b| a.name.cmp(&b.name));
    extensions
}

fn merge_listed_packages(
    home: &Path,
    mut settings_inventory: Vec<PiExtensionInfo>,
    listed: Vec<ListedPackage>,
) -> Vec<PiExtensionInfo> {
    let disabled = load_disabled(home);
    for package in listed {
        if settings_inventory.iter().any(|extension| {
            same_identity(
                &extension.source,
                extension.scope,
                extension.project_root.as_deref(),
                &package.source,
                package.scope,
                package.project_root.as_deref(),
            )
        }) {
            continue;
        }
        let (version, description) = store_metadata(home, &package.source);
        let disabled_record = disabled.iter().find(|record| {
            same_identity(
                &record.source,
                record.scope,
                record.project_root.as_deref(),
                &package.source,
                package.scope,
                package.project_root.as_deref(),
            )
        });
        settings_inventory.push(PiExtensionInfo {
            name: source_name(&package.source),
            version,
            description,
            scope: package.scope,
            // A source printed only by `pi list` is visible but not
            // Waku-managed. A matching disabled record is the exception: it
            // is a configured, restorable row even if settings no longer
            // contains the package entry.
            enabled: disabled_record.is_none(),
            manageable: disabled_record.is_some(),
            configured: disabled_record.is_some(),
            filtered: disabled_record
                .and_then(|record| record.entry.as_ref().map(entry_filters))
                .unwrap_or_default(),
            source: package.source,
            project_root: package.project_root,
        });
    }
    settings_inventory.sort_by(|a, b| a.name.cmp(&b.name));
    settings_inventory
}

/// Merge the daemon-host `pi list` result with the settings inventory. If
/// Pi is unavailable or the command fails, return settings-only data so an
/// inventory refresh never turns into a provider error.
pub fn load_extensions_with_pi_list(
    home: &Path,
    projects: &[(String, PathBuf)],
    binary: Option<&Path>,
) -> Vec<PiExtensionInfo> {
    let settings_inventory = load_extensions(home, projects);
    let Some(listed) = pi_list_inventory(binary, home, projects) else {
        return settings_inventory;
    };
    merge_listed_packages(home, settings_inventory, listed)
}

/// Rewrite one settings file's `packages` array in place, preserving every
/// other key and the entry object shapes.
fn write_packages(
    path: &Path,
    mutate: impl FnOnce(Vec<Value>) -> Option<Vec<Value>>,
) -> anyhow::Result<bool> {
    let bytes = fs::read(path).unwrap_or_else(|_| b"{}".into());
    let mut settings: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid pi settings at {}", path.display()))?;
    let object = settings
        .as_object_mut()
        .context("pi settings root is not an object")?;
    let packages = object
        .entry("packages")
        .or_insert_with(|| Value::Array(Vec::new()));
    let entries = packages
        .as_array_mut()
        .context("pi settings packages is not an array")?;
    let Some(next) = mutate(entries.clone()) else {
        return Ok(false);
    };
    *entries = next;
    if object["packages"].as_array().is_some_and(Vec::is_empty) {
        object.remove("packages");
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_string_pretty(&settings)?;
    fs::write(path, bytes)?;
    Ok(true)
}

/// Enable or disable one package. Disabling strips every matching entry from
/// the scope's settings and records the exact removed value; enabling restores
/// that value. The daemon-side record is scoped by source, scope, and project.
pub fn set_enabled(
    home: &Path,
    source: &str,
    scope: PiExtensionScope,
    project_root: Option<&Path>,
    enabled: bool,
) -> anyhow::Result<()> {
    let matches = |entry: &Value| entry_source(entry) == Some(source);
    let path = match scope {
        PiExtensionScope::User => user_settings_path(home),
        PiExtensionScope::Project => {
            let root = project_root.context("project-scope packages need a project root")?;
            project_settings_path(root)
        }
    };
    let mut disabled = load_disabled(home);
    let existing_entry = disabled
        .iter()
        .find(|record| {
            same_identity(
                &record.source,
                record.scope,
                record.project_root.as_deref(),
                source,
                scope,
                project_root,
            )
        })
        .and_then(|record| record.entry.clone());
    let current_entry = read_packages(&path).into_iter().find(matches);
    let entry_to_restore = existing_entry
        .clone()
        .unwrap_or_else(|| Value::String(source.to_owned()));

    write_packages(&path, |entries| {
        let original_len = entries.len();
        if enabled {
            if entries.iter().any(matches) {
                return None;
            }
            let mut next = entries;
            next.push(entry_to_restore);
            Some(next)
        } else {
            let next: Vec<Value> = entries
                .into_iter()
                .filter(|entry| !matches(entry))
                .collect();
            // An empty result is a legitimate write: the packages key is
            // dropped below. Only skip when nothing was actually removed.
            (next.len() != original_len).then_some(next)
        }
    })?;
    // The disabled record updates even when pi's settings had nothing to
    // change — a double-disable or a manually removed package must still
    // land in the manager's list.
    if enabled {
        disabled.retain(|record| {
            !same_identity(
                &record.source,
                record.scope,
                record.project_root.as_deref(),
                source,
                scope,
                project_root,
            )
        });
    } else {
        let record = DisabledPackage {
            source: source.to_owned(),
            scope,
            project_root: project_root.map(Path::to_path_buf),
            entry: current_entry
                .or(existing_entry)
                .or_else(|| Some(Value::String(source.to_owned()))),
        };
        if let Some(existing) = disabled.iter_mut().find(|existing| {
            same_identity(
                &existing.source,
                existing.scope,
                existing.project_root.as_deref(),
                source,
                scope,
                project_root,
            )
        }) {
            *existing = record;
        } else {
            disabled.push(record);
        }
    }
    save_disabled(home, &disabled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use uuid::Uuid;

    struct Sandbox {
        root: PathBuf,
    }

    impl Sandbox {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("waku-pi-settings-{}", Uuid::new_v4()));
            fs::create_dir_all(root.join(".pi/agent/npm/node_modules/pi-demo")).unwrap();
            fs::create_dir_all(root.join("project/.pi")).unwrap();
            Self { root }
        }

        fn home(&self) -> &Path {
            &self.root
        }

        fn write_user_settings(&self, packages: Value) {
            fs::write(
                self.root.join(".pi/agent/settings.json"),
                serde_json::to_string_pretty(&json!({ "packages": packages })).unwrap(),
            )
            .unwrap();
        }

        fn install_manifest(&self, version: &str, description: &str) {
            fs::write(
                self.root
                    .join(".pi/agent/npm/node_modules/pi-demo/package.json"),
                json!({ "name": "pi-demo", "version": version, "description": description })
                    .to_string(),
            )
            .unwrap();
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn pi_list_parser_keeps_sources_and_ignores_headers_and_paths() {
        let packages = parse_pi_list(
            "User packages:\n  npm:pi-demo (filtered)\n    /daemon/npm/pi-demo\n  extensions/pi-statusline\n    /daemon/extensions/pi-statusline\n\nProject packages:\n  npm:project-addon\n    /daemon/project-addon\n",
        );

        assert_eq!(
            packages,
            vec![
                ListedPackage {
                    source: "npm:pi-demo".to_owned(),
                    scope: PiExtensionScope::User,
                    project_root: None,
                },
                ListedPackage {
                    source: "extensions/pi-statusline".to_owned(),
                    scope: PiExtensionScope::User,
                    project_root: None,
                },
                ListedPackage {
                    source: "npm:project-addon".to_owned(),
                    scope: PiExtensionScope::Project,
                    project_root: None,
                },
            ]
        );
    }

    #[test]
    fn pi_list_sources_form_a_union_without_overwriting_settings_metadata() {
        let sandbox = Sandbox::new();
        sandbox.write_user_settings(json!([{
            "source": "npm:pi-demo",
            "extensions": ["-extensions/hidden.ts"]
        }]));
        sandbox.install_manifest("1.2.3", "demo package");

        let mut listed = parse_pi_list(
            "User packages:\n  npm:pi-demo (filtered)\n    /daemon/npm/pi-demo\n  npm:list-only\n    /daemon/npm/list-only\nProject packages:\n  extensions/pi-statusline\n    /daemon/extensions/pi-statusline\n",
        );
        let project_root = sandbox.root.join("project");
        listed[2].project_root = Some(project_root.clone());

        let merged =
            merge_listed_packages(sandbox.home(), load_extensions(sandbox.home(), &[]), listed);
        assert_eq!(merged.len(), 3);

        let configured = merged
            .iter()
            .find(|extension| extension.source == "npm:pi-demo")
            .expect("settings package remains in the union");
        assert_eq!(configured.version.as_deref(), Some("1.2.3"));
        assert_eq!(configured.description.as_deref(), Some("demo package"));
        assert_eq!(configured.scope, PiExtensionScope::User);
        assert!(configured.enabled);
        assert!(configured.manageable);
        assert!(configured.configured);
        assert_eq!(configured.filtered, vec!["extensions/hidden.ts"]);

        let list_only = merged
            .iter()
            .find(|extension| extension.source == "extensions/pi-statusline")
            .expect("pi list-only source is added");
        assert_eq!(list_only.name, "pi-statusline");
        assert_eq!(list_only.scope, PiExtensionScope::Project);
        assert_eq!(list_only.project_root.as_ref(), Some(&project_root));
        assert!(!list_only.manageable);
        assert!(!list_only.configured);
    }

    #[test]
    fn failed_pi_list_falls_back_to_settings_inventory() {
        let sandbox = Sandbox::new();
        sandbox.write_user_settings(json!(["npm:pi-demo"]));
        let expected = load_extensions(sandbox.home(), &[]);
        let missing_binary = sandbox.root.join("missing-pi");

        assert_eq!(
            load_extensions_with_pi_list(sandbox.home(), &[], Some(&missing_binary)),
            expected
        );
    }

    #[test]
    fn pi_list_matching_disabled_record_stays_manageable() {
        let sandbox = Sandbox::new();
        sandbox.write_user_settings(json!(["npm:pi-demo"]));
        set_enabled(
            sandbox.home(),
            "npm:pi-demo",
            PiExtensionScope::User,
            None,
            false,
        )
        .unwrap();

        let merged = merge_listed_packages(
            sandbox.home(),
            load_extensions(sandbox.home(), &[]),
            parse_pi_list("User packages:\n  npm:pi-demo\n"),
        );
        let extension = merged
            .iter()
            .find(|extension| extension.source == "npm:pi-demo")
            .expect("disabled package remains visible");
        assert!(!extension.enabled);
        assert!(extension.manageable);
        assert!(extension.configured);
    }

    #[test]
    fn settings_snapshot_reads_general_and_extension_values_per_scope() {
        let sandbox = Sandbox::new();
        let user_config = user_settings_path(sandbox.home());
        fs::write(
            &user_config,
            serde_json::to_string_pretty(&json!({
                "defaultProvider": "commandcode",
                "defaultModel": "deepseek/model",
                "defaultThinkingLevel": "high",
                "quietStartup": true,
                "other": {"kept": true}
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            global_extensions_settings_path(sandbox.home()),
            json!({
                "pi-demo": {"mode": "fast", "enabled": true},
                "pi-other": {"count": 2}
            })
            .to_string(),
        )
        .unwrap();

        let project = sandbox.root.join("project");
        fs::write(
            project_settings_path(&project),
            json!({
                "defaultModel": "project/model",
                "quietStartup": false
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            project_extensions_settings_path(&project),
            json!({"pi-demo": {"mode": "safe"}}).to_string(),
        )
        .unwrap();

        let snapshot =
            load_settings_snapshot(sandbox.home(), &[("Project".to_owned(), project.clone())]);
        assert_eq!(snapshot.global.config_path, user_config);
        assert_eq!(
            snapshot.global.default_provider.as_deref(),
            Some("commandcode")
        );
        assert_eq!(
            snapshot.global.default_model.as_deref(),
            Some("deepseek/model")
        );
        assert_eq!(
            snapshot.global.default_thinking_level.as_deref(),
            Some("high")
        );
        assert_eq!(snapshot.global.quiet_startup, Some(true));
        assert_eq!(snapshot.global.extension_settings.len(), 2);
        assert_eq!(snapshot.global.extension_settings[0].extension, "pi-demo");
        assert_eq!(
            snapshot.global.extension_settings[0]
                .entries
                .iter()
                .find(|entry| entry.key == "mode")
                .expect("mode entry")
                .value,
            Value::String("fast".to_owned())
        );
        assert_eq!(snapshot.projects.len(), 1);
        assert_eq!(snapshot.projects[0].name, "Project");
        assert_eq!(snapshot.projects[0].project_root, project);
        assert_eq!(
            snapshot.projects[0].settings.default_model.as_deref(),
            Some("project/model")
        );
        assert_eq!(
            snapshot.projects[0].settings.extension_settings[0].entries[0].value,
            Value::String("safe".to_owned())
        );
        assert_eq!(snapshot.global.error, None);
        assert_eq!(snapshot.projects[0].settings.error, None);
    }

    #[test]
    fn settings_snapshot_masks_and_bounds_extension_values() {
        let sandbox = Sandbox::new();
        let mut values = serde_json::Map::new();
        values.insert("api_key".to_owned(), json!("do-not-send"));
        values.insert("Auth-Token".to_owned(), json!("also-secret"));
        values.insert("private-key".to_owned(), json!("private-secret"));
        values.insert("ACCESS_KEY".to_owned(), json!("access-secret"));
        values.insert(
            "long".to_owned(),
            Value::String("x".repeat(MAX_EXTENSION_SETTING_STRING_CHARS + 20)),
        );
        values.insert(
            "array".to_owned(),
            Value::Array(
                (0..MAX_EXTENSION_SETTING_NESTED_ITEMS + 10)
                    .map(|value| Value::Number(value.into()))
                    .collect(),
            ),
        );
        let mut object = serde_json::Map::new();
        for index in 0..MAX_EXTENSION_SETTING_NESTED_ITEMS + 10 {
            object.insert(format!("key-{index}"), Value::Bool(true));
        }
        values.insert("object".to_owned(), Value::Object(object));
        let mut large_values = serde_json::Map::new();
        for index in 0..MAX_EXTENSION_SETTINGS_ENTRIES {
            large_values.insert(format!("entry-{index}"), Value::String("x".repeat(300)));
        }
        fs::write(
            global_extensions_settings_path(sandbox.home()),
            json!({
                "pi-demo": Value::Object(values),
                "pi-large": Value::Object(large_values)
            })
            .to_string(),
        )
        .unwrap();

        let snapshot = load_settings_snapshot(sandbox.home(), &[]);
        let entries = &snapshot.global.extension_settings[0].entries;
        let value = |key: &str| {
            &entries
                .iter()
                .find(|entry| entry.key == key)
                .expect("setting entry")
                .value
        };
        assert_eq!(
            value("api_key"),
            &Value::String(PI_SETTINGS_MASKED.to_owned())
        );
        assert_eq!(
            value("Auth-Token"),
            &Value::String(PI_SETTINGS_MASKED.to_owned())
        );
        assert_eq!(
            value("private-key"),
            &Value::String(PI_SETTINGS_MASKED.to_owned())
        );
        assert_eq!(
            value("ACCESS_KEY"),
            &Value::String(PI_SETTINGS_MASKED.to_owned())
        );
        assert!(
            value("long").as_str().unwrap().chars().count()
                <= MAX_EXTENSION_SETTING_STRING_CHARS + 1
        );
        assert!(value("array").as_array().unwrap().len() <= MAX_EXTENSION_SETTING_NESTED_ITEMS);
        assert!(value("object").as_object().unwrap().len() <= MAX_EXTENSION_SETTING_NESTED_ITEMS);
        assert!(entries.len() <= MAX_EXTENSION_SETTINGS_ENTRIES);
        assert!(
            serde_json::to_vec(&snapshot.global.extension_settings)
                .unwrap()
                .len()
                <= MAX_EXTENSION_SETTINGS_PAYLOAD_BYTES
        );
    }

    #[test]
    fn settings_snapshot_reports_malformed_scope_without_hiding_valid_project() {
        let sandbox = Sandbox::new();
        fs::write(user_settings_path(sandbox.home()), "{ malformed").unwrap();
        let project = sandbox.root.join("project");
        fs::write(
            project_settings_path(&project),
            json!({"defaultProvider": "project-provider"}).to_string(),
        )
        .unwrap();

        let snapshot = load_settings_snapshot(sandbox.home(), &[("Project".to_owned(), project)]);
        assert!(snapshot.global.error.is_some());
        assert_eq!(
            snapshot.projects[0].settings.default_provider.as_deref(),
            Some("project-provider")
        );
        assert_eq!(snapshot.projects[0].settings.error, None);
    }

    #[test]
    fn quiet_startup_write_preserves_unknown_global_settings() {
        let sandbox = Sandbox::new();
        sandbox.write_user_settings(json!(["npm:pi-demo"]));
        let path = user_settings_path(sandbox.home());
        let mut settings = json!({
            "defaultModel": "model",
            "packages": ["npm:pi-demo"],
            "nested": {"preserve": [1, 2, 3]}
        });
        fs::write(&path, serde_json::to_string_pretty(&settings).unwrap()).unwrap();

        set_quiet_startup(sandbox.home(), true).unwrap();
        settings = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(settings["quietStartup"], true);
        assert_eq!(settings["defaultModel"], "model");
        assert_eq!(settings["packages"], json!(["npm:pi-demo"]));
        assert_eq!(settings["nested"], json!({"preserve": [1, 2, 3]}));
    }

    #[cfg(unix)]
    #[test]
    fn pi_list_timeout_kills_hung_process() {
        use std::os::unix::fs::PermissionsExt;

        let sandbox = Sandbox::new();
        let binary = sandbox.root.join("slow-pi");
        fs::write(
            &binary,
            "#!/bin/sh\n/bin/sleep 30 &\nprintf 'User packages:\\n'\nexit 0\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&binary).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&binary, permissions).unwrap();

        let started = std::time::Instant::now();
        assert_eq!(
            list_pi_packages_with_timeout(
                &binary,
                sandbox.home(),
                std::time::Duration::from_millis(50),
            ),
            None
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn pi_extension_commands_use_scope_cwd_and_exact_args() {
        use std::os::unix::fs::PermissionsExt;

        let sandbox = Sandbox::new();
        sandbox.write_user_settings(json!(["npm:pi-demo"]));
        sandbox.install_manifest("1.2.3", "demo package");
        let binary = sandbox.root.join("fake-pi");
        fs::write(
            &binary,
            "#!/bin/sh\nif [ \"$1\" != list ]; then\nprintf '%s\\n' \"$@\" > \"$PWD/pi-args.log\"\nfi\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&binary).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&binary, permissions).unwrap();

        update_extension(
            &binary,
            sandbox.home(),
            "npm:pi-demo",
            PiExtensionScope::User,
            None,
            &[],
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(sandbox.root.join("pi-args.log")).unwrap(),
            "update\nnpm:pi-demo\n--approve\n"
        );

        let project = sandbox.root.join("project");
        fs::write(
            project_settings_path(&project),
            json!({"packages": ["npm:pi-demo"]}).to_string(),
        )
        .unwrap();
        remove_extension(
            &binary,
            sandbox.home(),
            "npm:pi-demo",
            PiExtensionScope::Project,
            Some(&project),
            &[("Project".to_owned(), project.clone())],
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(project.join("pi-args.log")).unwrap(),
            "remove\nnpm:pi-demo\n--local\n--approve\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pi_extension_mutations_require_valid_enabled_registered_identity() {
        use std::os::unix::fs::PermissionsExt;

        let sandbox = Sandbox::new();
        sandbox.write_user_settings(json!(["npm:pi-demo"]));
        sandbox.install_manifest("1.2.3", "demo package");
        let binary = sandbox.root.join("fake-pi");
        fs::write(
            &binary,
            "#!/bin/sh\nif [ \"$1\" != list ]; then\nprintf '%s\\n' \"$@\" > \"$PWD/pi-args.log\"\nfi\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&binary).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&binary, permissions).unwrap();

        assert!(
            update_extension(
                &binary,
                sandbox.home(),
                "--approve",
                PiExtensionScope::User,
                None,
                &[],
            )
            .is_err()
        );
        assert!(!sandbox.root.join("pi-args.log").exists());

        set_enabled(
            sandbox.home(),
            "npm:pi-demo",
            PiExtensionScope::User,
            None,
            false,
        )
        .unwrap();
        assert!(
            validate_extension_identity(
                Some(&binary),
                sandbox.home(),
                "npm:pi-demo",
                PiExtensionScope::User,
                None,
                &[],
                false,
            )
            .is_ok()
        );
        assert!(
            remove_extension(
                &binary,
                sandbox.home(),
                "npm:pi-demo",
                PiExtensionScope::User,
                None,
                &[],
            )
            .is_err()
        );
        assert!(!sandbox.root.join("pi-args.log").exists());

        let project = sandbox.root.join("project");
        fs::write(
            project_settings_path(&project),
            json!({"packages": ["npm:pi-demo"]}).to_string(),
        )
        .unwrap();
        let unregistered = sandbox.root.join("unregistered");
        fs::create_dir_all(unregistered.join(".pi")).unwrap();
        fs::write(
            project_settings_path(&unregistered),
            json!({"packages": ["npm:pi-demo"]}).to_string(),
        )
        .unwrap();
        assert!(
            update_extension(
                &binary,
                sandbox.home(),
                "npm:pi-demo",
                PiExtensionScope::Project,
                Some(&unregistered),
                &[("Project".to_owned(), project.clone())],
            )
            .is_err()
        );
        assert!(!unregistered.join("pi-args.log").exists());
        assert!(
            validate_extension_identity(
                Some(&binary),
                sandbox.home(),
                "npm:pi-demo",
                PiExtensionScope::Project,
                Some(&unregistered),
                &[("Project".to_owned(), project.clone())],
                false,
            )
            .is_err()
        );

        update_extension(
            &binary,
            sandbox.home(),
            "npm:pi-demo",
            PiExtensionScope::Project,
            Some(&project),
            &[("Project".to_owned(), project.clone())],
        )
        .unwrap();
    }

    #[test]
    fn pi_extension_source_validation_rejects_flags_and_unsupported_npm_values() {
        assert!(!valid_extension_source("--update"));
        assert!(!valid_extension_source("npm:pi demo"));
        assert!(!valid_extension_source("unknown:pi-demo"));
        assert!(valid_extension_source("npm:@scope/pi-demo"));
        assert!(valid_extension_source("extensions/pi-statusline"));
        assert!(!valid_mutation_source("npm:--registry"));
        assert!(valid_npm_source("npm:@scope/pi-demo").is_some());
        assert!(valid_npm_source("npm:--registry").is_none());
        assert!(valid_npm_source("npm:/tmp/pi-demo").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn pi_list_only_identity_cannot_be_authorized_for_mutation() {
        use std::os::unix::fs::PermissionsExt;

        let sandbox = Sandbox::new();
        let binary = sandbox.root.join("fake-pi");
        fs::write(
            &binary,
            "#!/bin/sh\nif [ \"$1\" = list ]; then\nprintf 'User packages:\\n  npm:list-only\\n'\nelse\nprintf '%s\\n' \"$@\" > \"$PWD/pi-args.log\"\nfi\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&binary).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&binary, permissions).unwrap();

        assert!(
            validate_extension_identity(
                Some(&binary),
                sandbox.home(),
                "npm:list-only",
                PiExtensionScope::User,
                None,
                &[],
                false,
            )
            .is_err()
        );
        assert!(
            update_extension(
                &binary,
                sandbox.home(),
                "npm:list-only",
                PiExtensionScope::User,
                None,
                &[],
            )
            .is_err()
        );
        assert!(
            remove_extension(
                &binary,
                sandbox.home(),
                "npm:list-only",
                PiExtensionScope::User,
                None,
                &[],
            )
            .is_err()
        );
        assert!(!sandbox.root.join("pi-args.log").exists());
    }

    #[cfg(unix)]
    #[test]
    fn npm_update_check_uses_package_argument_and_returns_version() {
        use std::os::unix::fs::PermissionsExt;

        let sandbox = Sandbox::new();
        let npm = sandbox.root.join("fake-npm");
        fs::write(
            &npm,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$PWD/npm-args.log\"\nprintf '1.2.4\\n'\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&npm).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&npm, permissions).unwrap();

        assert_eq!(
            check_extension_update_with_npm(
                &npm,
                sandbox.home(),
                "@scope/pi-demo",
                Duration::from_secs(1),
            )
            .as_deref(),
            Some("1.2.4")
        );
        assert_eq!(
            fs::read_to_string(sandbox.root.join("npm-args.log")).unwrap(),
            "view\n@scope/pi-demo\nversion\n"
        );
    }

    #[test]
    fn load_merges_user_and_project_scopes_with_versions() {
        let sandbox = Sandbox::new();
        sandbox.write_user_settings(json!(["npm:pi-demo"]));
        sandbox.install_manifest("1.2.3", "demo package");

        let extensions = load_extensions(sandbox.home(), &[]);
        assert_eq!(extensions.len(), 1);
        let extension = &extensions[0];
        assert_eq!(extension.source, "npm:pi-demo");
        assert_eq!(extension.name, "pi-demo");
        assert_eq!(extension.version.as_deref(), Some("1.2.3"));
        assert_eq!(extension.description.as_deref(), Some("demo package"));
        assert!(extension.enabled);
        assert_eq!(extension.scope, PiExtensionScope::User);

        // A disabled package stays visible (and re-enableable) while
        // installed, once pi's own settings no longer list it.
        sandbox.write_user_settings(json!(["npm:pi-other"]));
        set_enabled(
            sandbox.home(),
            "npm:pi-demo",
            PiExtensionScope::User,
            None,
            false,
        )
        .unwrap();
        let extensions = load_extensions(sandbox.home(), &[]);
        assert_eq!(extensions.len(), 2);
        let demo = extensions
            .iter()
            .find(|extension| extension.source == "npm:pi-demo")
            .expect("disabled pi-demo still listed");
        assert!(!demo.enabled);
        assert!(demo.manageable);
        assert!(demo.configured);
        assert!(
            extensions
                .iter()
                .find(|extension| extension.source == "npm:pi-other")
                .is_some_and(|extension| extension.enabled)
        );
        let _ = sandbox;
    }

    #[test]
    fn scoped_packages_resolve_inside_their_org_directory() {
        let sandbox = Sandbox::new();
        let org = sandbox
            .root
            .join(".pi/agent/npm/node_modules/@narumitw/pi-goal");
        fs::create_dir_all(&org).unwrap();
        fs::write(
            org.join("package.json"),
            json!({ "name": "@narumitw/pi-goal", "version": "0.53.1" }).to_string(),
        )
        .unwrap();
        sandbox.write_user_settings(json!(["npm:@narumitw/pi-goal"]));

        // Disabling strips the settings entry; the disabled row still
        // resolves its manifest through the scoped directory.
        set_enabled(
            sandbox.home(),
            "npm:@narumitw/pi-goal",
            PiExtensionScope::User,
            None,
            false,
        )
        .unwrap();
        let extensions = load_extensions(sandbox.home(), &[]);
        assert_eq!(extensions.len(), 1);
        assert!(!extensions[0].enabled);
        assert_eq!(extensions[0].name, "pi-goal");
        assert_eq!(extensions[0].version.as_deref(), Some("0.53.1"));
        let _ = sandbox;
    }

    #[test]
    fn disable_removes_the_entry_and_enable_restores_it() {
        let sandbox = Sandbox::new();
        sandbox.write_user_settings(json!(["npm:pi-demo", "npm:pi-other"]));

        set_enabled(
            sandbox.home(),
            "npm:pi-demo",
            PiExtensionScope::User,
            None,
            false,
        )
        .unwrap();
        let packages = read_packages(&user_settings_path(sandbox.home()));
        assert_eq!(packages.len(), 1);
        assert_eq!(entry_source(&packages[0]), Some("npm:pi-other"));

        set_enabled(
            sandbox.home(),
            "npm:pi-demo",
            PiExtensionScope::User,
            None,
            true,
        )
        .unwrap();
        let packages = read_packages(&user_settings_path(sandbox.home()));
        assert_eq!(packages.len(), 2);
        assert_eq!(entry_source(&packages[1]), Some("npm:pi-demo"));
        let _ = sandbox;
    }

    #[test]
    fn object_entries_and_other_keys_survive_a_disable() {
        let sandbox = Sandbox::new();
        let path = sandbox.root.join(".pi/agent/settings.json");
        let object_entry = json!({
            "source": "npm:pi-demo",
            "extensions": ["-extensions/todos/index.ts"],
            "autoload": false,
            "config": {"mode": "safe"}
        });
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "defaultModel": "x/y",
                "packages": [
                    object_entry.clone(),
                    "npm:pi-other"
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        set_enabled(
            sandbox.home(),
            "npm:pi-demo",
            PiExtensionScope::User,
            None,
            false,
        )
        .unwrap();
        let settings: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(settings["defaultModel"], "x/y");
        let packages = settings["packages"].as_array().unwrap();
        assert_eq!(packages.len(), 1);
        assert_eq!(entry_source(&packages[0]), Some("npm:pi-other"));

        set_enabled(
            sandbox.home(),
            "npm:pi-demo",
            PiExtensionScope::User,
            None,
            true,
        )
        .unwrap();
        let restored = read_packages(&path)
            .into_iter()
            .find(|entry| entry_source(entry) == Some("npm:pi-demo"))
            .expect("disabled package can be restored");
        assert_eq!(restored, object_entry);
        let _ = sandbox;
    }

    #[test]
    fn same_source_keeps_user_and_each_project_scope_distinct() {
        let sandbox = Sandbox::new();
        sandbox.write_user_settings(json!(["npm:pi-demo"]));
        sandbox.install_manifest("1.2.3", "demo package");
        let project_one = sandbox.root.join("project-one");
        let project_two = sandbox.root.join("project-two");
        for project in [&project_one, &project_two] {
            fs::create_dir_all(project.join(".pi")).unwrap();
            fs::write(
                project_settings_path(project),
                serde_json::to_string_pretty(&json!({
                    "packages": ["npm:pi-demo"]
                }))
                .unwrap(),
            )
            .unwrap();
        }
        let projects = vec![
            ("one".to_owned(), project_one.clone()),
            ("two".to_owned(), project_two.clone()),
        ];

        let extensions = load_extensions(sandbox.home(), &projects);
        assert_eq!(extensions.len(), 3);
        assert!(extensions.iter().any(|extension| {
            extension.source == "npm:pi-demo"
                && extension.scope == PiExtensionScope::User
                && extension.project_root.is_none()
                && extension.enabled
        }));
        assert!(extensions.iter().any(|extension| {
            extension.source == "npm:pi-demo"
                && extension.scope == PiExtensionScope::Project
                && extension.project_root.as_deref() == Some(project_one.as_path())
                && extension.enabled
        }));
        assert!(extensions.iter().any(|extension| {
            extension.source == "npm:pi-demo"
                && extension.scope == PiExtensionScope::Project
                && extension.project_root.as_deref() == Some(project_two.as_path())
                && extension.enabled
        }));

        set_enabled(
            sandbox.home(),
            "npm:pi-demo",
            PiExtensionScope::Project,
            Some(&project_one),
            false,
        )
        .unwrap();
        let extensions = load_extensions(sandbox.home(), &projects);
        assert!(extensions.iter().any(|extension| {
            extension.scope == PiExtensionScope::User
                && extension.project_root.is_none()
                && extension.enabled
        }));
        assert!(extensions.iter().any(|extension| {
            extension.scope == PiExtensionScope::Project
                && extension.project_root.as_deref() == Some(project_one.as_path())
                && !extension.enabled
        }));
        assert!(extensions.iter().any(|extension| {
            extension.scope == PiExtensionScope::Project
                && extension.project_root.as_deref() == Some(project_two.as_path())
                && extension.enabled
        }));
        let _ = sandbox;
    }

    #[test]
    fn project_scope_writes_the_project_file() {
        let sandbox = Sandbox::new();
        let project = sandbox.root.join("project");

        set_enabled(
            sandbox.home(),
            "npm:pi-demo",
            PiExtensionScope::Project,
            Some(&project),
            true,
        )
        .unwrap();
        let packages = read_packages(&project_settings_path(&project));
        assert_eq!(packages.len(), 1);
        assert_eq!(entry_source(&packages[0]), Some("npm:pi-demo"));
        let _ = sandbox;
    }
}

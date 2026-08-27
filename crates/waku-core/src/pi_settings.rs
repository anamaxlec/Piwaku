//! PIWAKU: pi package inventory and enable/disable, daemon-host side.
//!
//! Every path here belongs to the daemon host — the client's own `~/.pi` is
//! irrelevant when talking to a remote daemon. pi's settings `packages`
//! array is the single source of truth for what loads; it has no
//! whole-package disabled flag, so disabling removes the entry and the
//! daemon's own settings remember what to offer re-enabling.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde_json::Value;

use crate::model::{PiExtensionInfo, PiExtensionScope};

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

fn load_disabled(home: &Path) -> Vec<String> {
    let Ok(bytes) = fs::read(disabled_record_path(home)) else {
        return Vec::new();
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

fn save_disabled(home: &Path, disabled: &[String]) -> anyhow::Result<()> {
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
    fn seen(extensions: &[PiExtensionInfo], source: &str) -> bool {
        extensions
            .iter()
            .any(|extension| extension.source == source)
    }

    for entry in read_packages(&user_settings_path(home)) {
        let Some(source) = entry_source(&entry).map(str::to_owned) else {
            continue;
        };
        if seen(&extensions, &source) {
            continue;
        }
        let (version, description) = store_metadata(home, &source);
        extensions.push(PiExtensionInfo {
            name: source_name(&source),
            version,
            description,
            scope: PiExtensionScope::User,
            enabled: true,
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
            if seen(&extensions, &source) {
                continue;
            }
            let (version, description) = store_metadata(home, &source);
            extensions.push(PiExtensionInfo {
                name: source_name(&source),
                version,
                description,
                scope: PiExtensionScope::Project,
                enabled: true,
                filtered: entry_filters(&entry),
                source,
                project_root: Some(root.clone()),
            });
        }
    }

    for source in &disabled {
        if seen(&extensions, source) {
            continue;
        }
        // Only offer re-enabling what is still installed.
        let (version, _) = store_metadata(home, source);
        if version.is_none() {
            continue;
        }
        extensions.push(PiExtensionInfo {
            name: source_name(source),
            version,
            description: None,
            scope: PiExtensionScope::User,
            enabled: false,
            filtered: Vec::new(),
            source: source.clone(),
            project_root: None,
        });
    }

    extensions.sort_by(|a, b| a.name.cmp(&b.name));
    extensions
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
/// the scope's settings; enabling appends the plain source string. The
/// caller owns the daemon-side disabled record.
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
    write_packages(&path, |entries| {
        let original_len = entries.len();
        if enabled {
            if entries.iter().any(matches) {
                return None;
            }
            let mut next = entries;
            next.push(Value::String(source.to_owned()));
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
    let mut disabled = load_disabled(home);
    if enabled {
        disabled.retain(|entry| entry != source);
    } else if !disabled.contains(&source.to_owned()) {
        disabled.push(source.to_owned());
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
        fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "defaultModel": "x/y",
                "packages": [
                    {"source": "npm:pi-demo", "extensions": ["-extensions/todos/index.ts"]},
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

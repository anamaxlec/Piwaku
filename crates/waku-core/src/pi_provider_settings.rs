//! Credential-blind CRUD for Pi's daemon-host `models.json`.
//!
//! This module intentionally does not touch Pi's `auth.json`.  Pi built-in and
//! OAuth providers are consequently outside this v1 surface; only explicit
//! entries in `models.json` can be changed.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use anyhow::{Context as _, anyhow, bail};
use parking_lot::{Mutex, MutexGuard};
use serde_json::{Map, Value};
use url::Url;
use uuid::Uuid;
use waku_protocol::model::{
    PiApiKeyUpdate, PiModelSnapshot, PiProviderSettingsSnapshot, PiProviderSnapshot,
};

const SUPPORTED_APIS: &[&str] = &[
    "openai-completions",
    "openai-responses",
    "anthropic-messages",
    "google-generative-ai",
];
const MAX_ID_CHARS: usize = 128;
const MAX_TEXT_CHARS: usize = 512;

fn mutation_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct MutationGuard {
    _process: MutexGuard<'static, ()>,
    _file: fs::File,
}

fn mutation_guard(home: &Path) -> anyhow::Result<MutationGuard> {
    let process = mutation_lock().lock();
    let path = models_path(home);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("models.json has no parent directory"))?;
    fs::create_dir_all(parent)?;
    let lock_path = parent.join(".waku-models.lock");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options
        .open(&lock_path)
        .with_context(|| format!("could not open {}", lock_path.display()))?;
    file.lock()
        .with_context(|| format!("could not lock {}", lock_path.display()))?;
    Ok(MutationGuard {
        _process: process,
        _file: file,
    })
}

pub fn models_path(home: &Path) -> PathBuf {
    models_path_with_agent_dir(home, std::env::var_os("PI_CODING_AGENT_DIR").as_deref())
}

fn models_path_with_agent_dir(home: &Path, agent_dir: Option<&OsStr>) -> PathBuf {
    agent_dir
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".pi").join("agent"))
        .join("models.json")
}

pub fn load(home: &Path) -> PiProviderSettingsSnapshot {
    let path = models_path(home);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return PiProviderSettingsSnapshot {
                models_path: path,
                providers: Vec::new(),
                error: None,
            };
        }
        Err(error) => {
            return error_snapshot(path, error.to_string());
        }
    };
    let value = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => value,
        Err(error) => return error_snapshot(path, format!("models.json is invalid JSON: {error}")),
    };
    match parse_snapshot(path.clone(), &value) {
        Ok(providers) => PiProviderSettingsSnapshot {
            models_path: path,
            providers,
            error: None,
        },
        Err(error) => error_snapshot(path, error.to_string()),
    }
}

pub fn upsert_provider(
    home: &Path,
    id: &str,
    name: Option<&str>,
    base_url: Option<&str>,
    api: &str,
    api_key: PiApiKeyUpdate,
) -> anyhow::Result<()> {
    let _lock = mutation_guard(home)?;
    validate_provider_id(id)?;
    validate_text_option(name, "provider name")?;
    let base_url = validate_provider_base_url(base_url)?;
    validate_api(api)?;
    let (mut root, stamp) = read_root(home)?;
    let providers = root
        .get_mut("providers")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow!("models.json providers must be an object"))?;
    let is_new = !providers.contains_key(id);
    let provider = providers
        .entry(id.to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    let provider = provider
        .as_object_mut()
        .ok_or_else(|| anyhow!("provider {id:?} must be an object"))?;
    if !is_new {
        ensure_writable_provider(provider, id)?;
    }
    set_optional_string(provider, "name", name);
    provider.insert("baseUrl".to_owned(), Value::String(base_url.to_owned()));
    provider.insert("api".to_owned(), Value::String(api.to_owned()));
    if !provider.contains_key("models") {
        provider.insert("models".to_owned(), Value::Array(Vec::new()));
    }
    match api_key {
        PiApiKeyUpdate::Unchanged => {}
        PiApiKeyUpdate::Replace(value) => {
            if value.trim().is_empty() {
                bail!("api key replacement must not be empty");
            }
            provider.insert("apiKey".to_owned(), Value::String(value));
        }
        PiApiKeyUpdate::Clear => {
            provider.remove("apiKey");
        }
    }
    write_root(home, &root, &stamp)
}

pub fn delete_provider(home: &Path, id: &str) -> anyhow::Result<()> {
    let _lock = mutation_guard(home)?;
    validate_provider_id(id)?;
    let (mut root, stamp) = read_root(home)?;
    let providers = root
        .get_mut("providers")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow!("models.json providers must be an object"))?;
    let Some(provider) = providers.get(id) else {
        bail!("provider {id:?} does not exist");
    };
    let provider = provider
        .as_object()
        .ok_or_else(|| anyhow!("provider {id:?} must be an object"))?;
    ensure_writable_provider(provider, id)?;
    if provider
        .get("models")
        .and_then(Value::as_array)
        .is_some_and(|models| !models.is_empty())
    {
        bail!("provider {id:?} still has models; delete its models first");
    }
    providers.remove(id);
    write_root(home, &root, &stamp)
}

pub fn upsert_model(home: &Path, provider_id: &str, model: &PiModelSnapshot) -> anyhow::Result<()> {
    let _lock = mutation_guard(home)?;
    validate_provider_id(provider_id)?;
    validate_model_id(&model.id)?;
    validate_text(&model.name, "model name")?;
    validate_input(&model.input)?;
    if let Some(api) = model.api.as_deref() {
        validate_api(api)?;
    }
    let (mut root, stamp) = read_root(home)?;
    let providers = root
        .get_mut("providers")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow!("models.json providers must be an object"))?;
    let provider = providers
        .get_mut(provider_id)
        .ok_or_else(|| anyhow!("provider {provider_id:?} does not exist"))?
        .as_object_mut()
        .ok_or_else(|| anyhow!("provider {provider_id:?} must be an object"))?;
    ensure_writable_provider(provider, provider_id)?;
    let models = provider
        .entry("models".to_owned())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| anyhow!("provider {provider_id:?} models must be an array"))?;
    let index = models
        .iter()
        .position(|value| value.get("id").and_then(Value::as_str) == Some(model.id.as_str()));
    if let Some(index) = index {
        ensure_writable_model(&models[index], &model.id)?;
    }
    if index.is_none() {
        models.push(Value::Object(Map::new()));
    }
    let target_index = index.unwrap_or(models.len() - 1);
    let target = models[target_index]
        .as_object_mut()
        .ok_or_else(|| anyhow!("model {:?} must be an object", model.id))?;
    target.insert("id".to_owned(), Value::String(model.id.clone()));
    target.insert("name".to_owned(), Value::String(model.name.clone()));
    set_optional_string(target, "api", model.api.as_deref());
    set_optional_bool(target, "reasoning", model.reasoning);
    target.insert("input".to_owned(), input_to_value(&model.input));
    set_optional_u64(target, "contextWindow", model.context_window);
    set_optional_u64(target, "maxTokens", model.max_tokens);
    write_root(home, &root, &stamp)
}

pub fn delete_model(home: &Path, provider_id: &str, model_id: &str) -> anyhow::Result<()> {
    let _lock = mutation_guard(home)?;
    validate_provider_id(provider_id)?;
    validate_model_id(model_id)?;
    let (mut root, stamp) = read_root(home)?;
    let providers = root
        .get_mut("providers")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow!("models.json providers must be an object"))?;
    let provider = providers
        .get_mut(provider_id)
        .ok_or_else(|| anyhow!("provider {provider_id:?} does not exist"))?
        .as_object_mut()
        .ok_or_else(|| anyhow!("provider {provider_id:?} must be an object"))?;
    ensure_writable_provider(provider, provider_id)?;
    let models = provider
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| anyhow!("provider {provider_id:?} models must be an array"))?;
    let Some(index) = models
        .iter()
        .position(|value| value.get("id").and_then(Value::as_str) == Some(model_id))
    else {
        bail!("model {model_id:?} does not exist");
    };
    ensure_writable_model(&models[index], model_id)?;
    models.remove(index);
    write_root(home, &root, &stamp)
}

fn parse_snapshot(path: PathBuf, root: &Value) -> anyhow::Result<Vec<PiProviderSnapshot>> {
    let providers = root
        .get("providers")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("{}: providers must be an object", path.display()))?;
    providers
        .iter()
        .map(|(id, value)| parse_provider(id, value))
        .collect()
}

fn parse_provider(id: &str, value: &Value) -> anyhow::Result<PiProviderSnapshot> {
    let provider = value
        .as_object()
        .ok_or_else(|| anyhow!("provider {id:?} must be an object"))?;
    let api = provider
        .get("api")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let oauth = provider.contains_key("oauth");
    let models = match provider.get("models") {
        Some(models) => models
            .as_array()
            .map(Vec::as_slice)
            .ok_or_else(|| anyhow!("provider {id:?} models must be an array"))?,
        None => &[][..],
    };
    let models = models
        .iter()
        .map(|model| parse_model(model, api.as_deref()))
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(PiProviderSnapshot {
        id: id.to_owned(),
        name: provider
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned),
        base_url: provider
            .get("baseUrl")
            .and_then(Value::as_str)
            .map(str::to_owned),
        api: api.clone(),
        api_key_configured: provider
            .get("apiKey")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty()),
        read_only: oauth
            || api
                .as_deref()
                .is_none_or(|api| !SUPPORTED_APIS.contains(&api)),
        models,
    })
}

fn parse_model(value: &Value, provider_api: Option<&str>) -> anyhow::Result<PiModelSnapshot> {
    let model = value
        .as_object()
        .ok_or_else(|| anyhow!("model entry must be an object"))?;
    let id = model
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow!("model entry has no id"))?;
    let name = model
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(id)
        .to_owned();
    let input = match model.get("input") {
        Some(Value::Array(values))
            if values.iter().any(|value| value.as_str() == Some("image")) =>
        {
            "text+image"
        }
        _ => "text",
    };
    let api = model.get("api").and_then(Value::as_str).map(str::to_owned);
    let read_only = match model.get("api") {
        Some(Value::String(api)) => !SUPPORTED_APIS.contains(&api.as_str()),
        Some(_) => true,
        None => provider_api.is_none_or(|api| !SUPPORTED_APIS.contains(&api)),
    };
    Ok(PiModelSnapshot {
        id: id.to_owned(),
        name,
        api,
        reasoning: model.get("reasoning").and_then(Value::as_bool),
        input: input.to_owned(),
        context_window: model.get("contextWindow").and_then(Value::as_u64),
        max_tokens: model.get("maxTokens").and_then(Value::as_u64),
        read_only,
    })
}

fn ensure_writable_provider(provider: &Map<String, Value>, id: &str) -> anyhow::Result<()> {
    if provider.contains_key("oauth") {
        bail!("provider {id:?} uses OAuth and is read-only");
    }
    let Some(api) = provider.get("api").and_then(Value::as_str) else {
        bail!("provider {id:?} has no supported API and is read-only");
    };
    if !SUPPORTED_APIS.contains(&api) {
        bail!("provider {id:?} uses unsupported API {api:?} and is read-only");
    }
    Ok(())
}

fn ensure_writable_model(model: &Value, id: &str) -> anyhow::Result<()> {
    let model = model
        .as_object()
        .ok_or_else(|| anyhow!("model {id:?} must be an object"))?;
    match model.get("api") {
        None => Ok(()),
        Some(Value::String(api)) if SUPPORTED_APIS.contains(&api.as_str()) => Ok(()),
        Some(Value::String(api)) => {
            bail!("model {id:?} uses unsupported API {api:?} and is read-only")
        }
        Some(_) => bail!("model {id:?} has an invalid API and is read-only"),
    }
}

fn error_snapshot(path: PathBuf, error: String) -> PiProviderSettingsSnapshot {
    PiProviderSettingsSnapshot {
        models_path: path,
        providers: Vec::new(),
        error: Some(error),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DocumentStamp {
    exists: bool,
    len: u64,
    modified: Option<SystemTime>,
}

fn document_stamp(path: &Path) -> anyhow::Result<DocumentStamp> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(DocumentStamp {
            exists: true,
            len: metadata.len(),
            modified: Some(metadata.modified().with_context(|| {
                format!("could not read modification time for {}", path.display())
            })?),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(DocumentStamp {
            exists: false,
            len: 0,
            modified: None,
        }),
        Err(error) => Err(error.into()),
    }
}

fn read_root(home: &Path) -> anyhow::Result<(Value, DocumentStamp)> {
    let path = models_path(home);
    let before = document_stamp(&path)?;
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let after = document_stamp(&path)?;
            if before != after {
                bail!("models.json changed while reading; refresh and retry");
            }
            return Ok((serde_json::json!({"providers": {}}), after));
        }
        Err(error) => return Err(error.into()),
    };
    let stamp = document_stamp(&path)?;
    if before != stamp {
        bail!("models.json changed while reading; refresh and retry");
    }
    let root: Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("{} is invalid JSON", path.display()))?;
    if !root.is_object() {
        bail!("{} root must be an object", path.display());
    }
    if !root.get("providers").is_some_and(Value::is_object) {
        bail!("{} providers must be an object", path.display());
    }
    Ok((root, stamp))
}

fn write_root(home: &Path, root: &Value, expected: &DocumentStamp) -> anyhow::Result<()> {
    let path = models_path(home);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("models.json has no parent directory"))?;
    #[cfg(unix)]
    let existing_mode = fs::metadata(&path)
        .ok()
        .map(|metadata| metadata.permissions().mode());
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".waku-models-{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("could not create {}", temporary.display()))?;
    let result = (|| -> anyhow::Result<()> {
        #[cfg(unix)]
        if let Some(mode) = existing_mode {
            fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))?;
        }
        serde_json::to_writer_pretty(&mut file, root)?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_all()?;
        let current = document_stamp(&path)?;
        if current != *expected {
            bail!("models.json changed while editing; refresh and retry");
        }
        fs::rename(&temporary, &path)?;
        #[cfg(unix)]
        if let Ok(directory) = OpenOptions::new().read(true).open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn validate_id(value: &str, label: &str) -> anyhow::Result<()> {
    if value.trim().is_empty()
        || value.chars().count() > MAX_ID_CHARS
        || value.starts_with('-')
        || value.chars().any(char::is_control)
    {
        bail!("{label} is invalid");
    }
    Ok(())
}

fn validate_provider_id(value: &str) -> anyhow::Result<()> {
    validate_id(value, "provider id")?;
    if value != value.trim() || value.contains('/') {
        bail!("provider id is invalid");
    }
    Ok(())
}

fn validate_model_id(value: &str) -> anyhow::Result<()> {
    validate_id(value, "model id")
}

fn validate_text(value: &str, label: &str) -> anyhow::Result<()> {
    if value.trim().is_empty()
        || value.chars().count() > MAX_TEXT_CHARS
        || value.chars().any(char::is_control)
    {
        bail!("{label} is invalid");
    }
    Ok(())
}

fn validate_text_option(value: Option<&str>, label: &str) -> anyhow::Result<()> {
    if let Some(value) = value {
        validate_text(value, label)?;
    }
    Ok(())
}

fn validate_provider_base_url(value: Option<&str>) -> anyhow::Result<&str> {
    let value = value.ok_or_else(|| anyhow!("provider base URL is required"))?;
    validate_text(value, "provider base URL")?;
    let url = Url::parse(value).with_context(|| "provider base URL is invalid")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("provider base URL must use http or https");
    }
    Ok(value)
}

fn validate_api(api: &str) -> anyhow::Result<()> {
    if SUPPORTED_APIS.contains(&api) {
        Ok(())
    } else {
        bail!("unsupported Pi model API {api:?}");
    }
}

fn validate_input(input: &str) -> anyhow::Result<()> {
    if matches!(input, "text" | "text+image") {
        Ok(())
    } else {
        bail!("model input must be text or text+image");
    }
}

fn set_optional_string(target: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    match value {
        Some(value) => {
            target.insert(key.to_owned(), Value::String(value.to_owned()));
        }
        None => {
            target.remove(key);
        }
    }
}

fn set_optional_bool(target: &mut Map<String, Value>, key: &str, value: Option<bool>) {
    match value {
        Some(value) => {
            target.insert(key.to_owned(), Value::Bool(value));
        }
        None => {
            target.remove(key);
        }
    }
}

fn set_optional_u64(target: &mut Map<String, Value>, key: &str, value: Option<u64>) {
    match value {
        Some(value) => {
            target.insert(key.to_owned(), Value::Number(value.into()));
        }
        None => {
            target.remove(key);
        }
    }
}

fn input_to_value(input: &str) -> Value {
    if input == "text+image" {
        Value::Array(vec![
            Value::String("text".into()),
            Value::String("image".into()),
        ])
    } else {
        Value::Array(vec![Value::String("text".into())])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    fn home() -> PathBuf {
        std::env::temp_dir().join(format!("waku-pi-models-{}", Uuid::new_v4()))
    }

    fn model(id: &str) -> PiModelSnapshot {
        PiModelSnapshot {
            id: id.into(),
            name: id.into(),
            api: None,
            read_only: false,
            reasoning: Some(true),
            input: "text".into(),
            context_window: Some(100),
            max_tokens: Some(20),
        }
    }

    #[test]
    fn snapshot_blinds_keys_and_marks_unknown_api_read_only() {
        let root = home();
        let path = models_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "future": 1,
                "providers": {
                    "custom": {"name":"Custom","api":"openai-responses","apiKey":"do-not-return","models":[]},
                    "builtin": {"api":"future-api","models":[]}
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let snapshot = load(&root);
        assert_eq!(snapshot.error, None);
        assert!(
            snapshot
                .providers
                .iter()
                .find(|p| p.id == "custom")
                .unwrap()
                .api_key_configured
        );
        assert!(
            snapshot
                .providers
                .iter()
                .find(|p| p.id == "builtin")
                .unwrap()
                .read_only
        );
        assert!(
            !serde_json::to_string(&snapshot)
                .unwrap()
                .contains("do-not-return")
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn models_path_prefers_pi_agent_dir_without_mutating_process_env() {
        let home = Path::new("/home/example");
        assert_eq!(
            models_path_with_agent_dir(home, Some(OsStr::new("/custom/pi-agent"))),
            PathBuf::from("/custom/pi-agent/models.json")
        );
        assert_eq!(
            models_path_with_agent_dir(home, Some(OsStr::new(""))),
            PathBuf::from("/home/example/.pi/agent/models.json")
        );
    }

    #[test]
    fn crud_preserves_unknown_fields_and_requires_empty_provider_for_delete() {
        let root = home();
        let path = models_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"rootFuture":true,"providers":{"p":{"api":"openai-responses","future":42,"models":[]}}}"#).unwrap();
        upsert_provider(
            &root,
            "p",
            Some("P"),
            Some("https://example.test"),
            "openai-responses",
            PiApiKeyUpdate::Replace("secret".into()),
        )
        .unwrap();
        upsert_model(&root, "p", &model("m")).unwrap();
        assert!(delete_provider(&root, "p").is_err());
        delete_model(&root, "p", "m").unwrap();
        delete_provider(&root, "p").unwrap();
        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["rootFuture"], true);
        assert_eq!(value["providers"], json!({}));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn malformed_json_never_writes() {
        let root = home();
        let path = models_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"not json").unwrap();
        assert!(
            upsert_provider(
                &root,
                "p",
                None,
                Some("https://example.test"),
                "openai-responses",
                PiApiKeyUpdate::Unchanged
            )
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"not json");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn built_in_entries_are_visible_but_not_writable() {
        let root = home();
        let path = models_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{
                "providers": {
                    "built-in": {"models": []},
                    "oauth": {"api":"openai-responses","oauth":"radius","models":[]},
                    "model-overrides": {"modelOverrides": {"model": "other"}}
                }
            }"#,
        )
        .unwrap();

        let snapshot = load(&root);
        assert_eq!(snapshot.error, None);
        assert_eq!(snapshot.providers.len(), 3);
        assert!(snapshot.providers.iter().all(|provider| provider.read_only));
        assert!(
            upsert_provider(
                &root,
                "built-in",
                None,
                Some("https://example.test"),
                "openai-responses",
                PiApiKeyUpdate::Unchanged,
            )
            .is_err()
        );
        assert!(upsert_model(&root, "built-in", &model("m")).is_err());
        assert!(delete_provider(&root, "built-in").is_err());
        assert!(
            upsert_provider(
                &root,
                "oauth",
                None,
                Some("https://example.test"),
                "openai-responses",
                PiApiKeyUpdate::Unchanged,
            )
            .is_err()
        );
        assert!(delete_provider(&root, "oauth").is_err());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn unknown_model_api_is_visible_but_not_writable() {
        let root = home();
        let path = models_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{"providers":{"p":{"api":"openai-responses","models":[{"id":"m","name":"M","api":"future-api"},{"id":"inherited","name":"Inherited"}]}}}"#,
        )
        .unwrap();
        let snapshot = load(&root);
        let provider = snapshot
            .providers
            .iter()
            .find(|provider| provider.id == "p")
            .unwrap();
        assert!(
            provider
                .models
                .iter()
                .find(|model| model.id == "m")
                .unwrap()
                .read_only
        );
        assert!(
            !provider
                .models
                .iter()
                .find(|model| model.id == "inherited")
                .unwrap()
                .read_only
        );
        assert!(upsert_model(&root, "p", &model("m")).is_err());
        assert!(delete_model(&root, "p", "m").is_err());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn provider_ids_and_base_urls_are_validated_without_restricting_model_slashes() {
        let root = home();
        let path = models_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"providers":{"p":{"api":"openai-responses","baseUrl":"https://example.test","models":[]}}}"#,
        )
        .unwrap();

        assert!(upsert_model(&root, "p", &model("nested/model")).is_ok());
        assert!(
            upsert_provider(
                &root,
                "bad/provider",
                None,
                Some("https://example.test"),
                "openai-responses",
                PiApiKeyUpdate::Unchanged,
            )
            .is_err()
        );
        assert!(
            upsert_provider(
                &root,
                " bad",
                None,
                Some("https://example.test"),
                "openai-responses",
                PiApiKeyUpdate::Unchanged,
            )
            .is_err()
        );
        for base_url in [None, Some("ftp://example.test"), Some("not a URL")] {
            assert!(
                upsert_provider(
                    &root,
                    "p",
                    None,
                    base_url,
                    "openai-responses",
                    PiApiKeyUpdate::Unchanged,
                )
                .is_err()
            );
        }
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn mutation_rejects_models_json_changed_since_read() {
        let root = home();
        let path = models_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"providers":{}}"#).unwrap();
        let (root_value, stamp) = read_root(&root).unwrap();
        fs::write(
            &path,
            r#"{"providers":{"external":{"api":"openai-responses","baseUrl":"https://external.test","models":[]}}}"#,
        )
        .unwrap();
        assert_ne!(document_stamp(&path).unwrap(), stamp);
        assert!(write_root(&root, &root_value, &stamp).is_err());
        let current: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(current["providers"].get("external").is_some());
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn mutation_file_lock_serializes_independent_handles() {
        let root = home();
        let path = models_path(&root);
        let parent = path.parent().unwrap();
        fs::create_dir_all(parent).unwrap();
        let lock_path = parent.join(".waku-models.lock");
        let first = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        first.lock().unwrap();
        let second = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (locked_tx, locked_rx) = mpsc::channel();
        let waiter = thread::spawn(move || {
            started_tx.send(()).unwrap();
            second.lock().unwrap();
            locked_tx.send(()).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(locked_rx.recv_timeout(Duration::from_millis(100)).is_err());
        first.unlock().unwrap();
        locked_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        waiter.join().unwrap();
        fs::remove_dir_all(root).ok();
    }

    #[test]
    #[cfg(unix)]
    fn mutation_preserves_existing_private_mode() {
        use std::fs::Permissions;
        use std::os::unix::fs::PermissionsExt;

        let root = home();
        let path = models_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"providers":{"p":{"api":"openai-responses","models":[]}}}"#,
        )
        .unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
        upsert_provider(
            &root,
            "p",
            Some("Provider"),
            Some("https://example.test"),
            "openai-responses",
            PiApiKeyUpdate::Unchanged,
        )
        .unwrap();
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(root).ok();
    }
}

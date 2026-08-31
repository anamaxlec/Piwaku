//! PIWAKU: the Settings → Pi page — the installed pi extensions manager.
//!
//! The inventory lives on the daemon host (a remote client's `~/.pi` is
//! irrelevant); the page only renders what [`Command::LoadPiExtensions`]
//! returned and flips packages through [`Command::SetPiExtensionEnabled`].
//! Compatibility badges are Piwaku's own hardcoded metadata — the installed
//! state always comes from the daemon.

use super::*;
use crate::model::{
    PiApiKeyUpdate, PiExtensionScope, PiExtensionSettingsGroup, PiModelSnapshot,
    PiSettingsScopeSnapshot, ProviderKind,
};
use serde_json::Value;
use std::cmp::Ordering;

/// How long a cached pi extension inventory stays trusted.
const PI_EXTENSIONS_RESCAN_AFTER: std::time::Duration = std::time::Duration::from_secs(15);
const PI_SETTING_VALUE_MAX_CHARS: usize = 180;

/// Sources with a dedicated Piwaku adapter or a deliberate non-adapter
/// stance. Anything absent renders as generic-compatible.
fn compatibility(source: &str) -> PiCompatibility {
    match source {
        "npm:@juicesharp/rpiv-ask-user-question"
        | "npm:@juicesharp/rpiv-todo"
        | "npm:@gotgenes/pi-permission-system"
        | "npm:@narumitw/pi-plan-mode"
        | "npm:@cortexkit/pi-magic-context" => PiCompatibility::Native,
        "npm:pi-tool-display" => PiCompatibility::Replaced,
        "npm:@victor-software-house/pi-curated-themes" => PiCompatibility::TuiOnly,
        _ => PiCompatibility::Generic,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PiCompatibility {
    Native,
    Replaced,
    TuiOnly,
    Generic,
}

impl PiCompatibility {
    fn label(self) -> String {
        match self {
            Self::Native => tr!("pi_ext.compat_native"),
            Self::Replaced => tr!("pi_ext.compat_replaced"),
            Self::TuiOnly => tr!("pi_ext.compat_tui_only"),
            Self::Generic => tr!("pi_ext.compat_generic"),
        }
    }

    fn emphasized(self) -> bool {
        self == Self::Native
    }
}

impl Waku {
    // ── Inventory ──────────────────────────────────────────────────────────

    /// Start a background inventory load unless one is current or in flight.
    /// Generation-guarded like the skills catalog: a toggle supersedes the
    /// scan it triggered.
    pub(super) fn ensure_pi_extensions(&mut self, force: bool, cx: &mut Context<Self>) {
        if self.pi_extensions_pending {
            return;
        }
        // A short TTL keeps the manager honest: daemon hot-swaps and
        // out-of-band `pi install` calls change the inventory without the
        // app hearing about it, so a cached list must age out.
        let fresh = self
            .pi_extensions_scanned_at
            .is_some_and(|scanned| scanned.elapsed() < PI_EXTENSIONS_RESCAN_AFTER);
        if !force && self.pi_extensions.is_some() && fresh {
            return;
        }
        self.pi_extensions_pending = true;
        self.pi_extensions_generation += 1;
        let generation = self.pi_extensions_generation;
        self.pi_extensions_inflight_generation = Some(generation);
        let projects = self.skill_scan_projects();
        let daemon = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let extensions = cx
                .background_executor()
                .spawn(async move {
                    match daemon.request(
                        Uuid::nil(),
                        Uuid::nil(),
                        waku_client::Command::LoadPiExtensions { projects },
                    ) {
                        Ok(waku_client::ResponsePayload::PiExtensions { extensions }) => {
                            Ok(extensions)
                        }
                        Ok(_) => {
                            anyhow::bail!("the daemon returned an invalid pi extensions response")
                        }
                        Err(error) => Err(error),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.pi_extensions_inflight_generation != Some(generation) {
                    return;
                }
                this.pi_extensions_inflight_generation = None;
                this.pi_extensions_pending = false;
                if this.pi_extensions_generation != generation {
                    // A toggle may have invalidated this scan while its RPC
                    // was in flight. Let whichever operation finishes last
                    // start exactly one fresh scan.
                    if !this.pi_extensions_mutation_pending {
                        this.ensure_pi_extensions(true, cx);
                    }
                    cx.notify();
                    return;
                }
                match extensions {
                    Ok(extensions) => {
                        this.pi_extensions = Some(Rc::new(extensions));
                        this.pi_extensions_scanned_at = Some(Instant::now());
                        this.pi_extension_latest_versions.clear();
                        this.pi_extension_remove_arming = None;
                    }
                    Err(error) => this.show_toast(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn set_pi_extension_enabled(
        &mut self,
        source: String,
        scope: PiExtensionScope,
        project_root: Option<PathBuf>,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        if self.pi_extensions_mutation_pending
            || self.pi_extension_action_pending
            || !self.pi_extension_update_pending.is_empty()
        {
            return;
        }
        self.pi_extensions_mutation_pending = true;
        self.pi_extensions_generation += 1;
        let daemon = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let result = daemon
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    waku_client::Command::SetPiExtensionEnabled {
                        source: source.clone(),
                        scope,
                        project_root,
                        enabled,
                    },
                )
                .map(|payload| match payload {
                    waku_client::ResponsePayload::Ack => Ok(()),
                    _ => anyhow::bail!("the daemon returned an invalid ack"),
                });
            let _ = this.update(cx, |this, cx| {
                if let Err(error) = result {
                    this.show_toast(error.to_string());
                }
                this.pi_extensions_mutation_pending = false;
                this.ensure_pi_extensions(true, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn check_pi_extension_update(
        &mut self,
        source: String,
        identity: String,
        cx: &mut Context<Self>,
    ) {
        if !can_check_pi_extension(&source)
            || self.pi_extensions_pending
            || self.pi_extension_action_pending
            || !self.pi_extension_update_pending.insert(identity.clone())
        {
            return;
        }
        let generation = self.pi_extensions_generation;
        let daemon = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let result = daemon.request(
                Uuid::nil(),
                Uuid::nil(),
                waku_client::Command::CheckPiExtensionUpdate {
                    source: source.clone(),
                },
            );
            let _ = this.update(cx, |this, cx| {
                this.pi_extension_update_pending.remove(&identity);
                if this.pi_extensions_generation != generation {
                    cx.notify();
                    return;
                }
                let latest_version = match result {
                    Ok(waku_client::ResponsePayload::PiExtensionUpdateCheck {
                        source: response_source,
                        latest_version,
                    }) if response_source == source
                        && latest_version
                            .as_deref()
                            .is_some_and(|version| parse_pi_semver(version).is_some()) =>
                    {
                        latest_version
                    }
                    _ => None,
                };
                this.pi_extension_latest_versions
                    .insert(identity, latest_version);
                cx.notify();
            });
        })
        .detach();
    }

    fn run_pi_extension_action(
        &mut self,
        source: String,
        scope: PiExtensionScope,
        project_root: Option<PathBuf>,
        identity: String,
        remove: bool,
        cx: &mut Context<Self>,
    ) {
        if self.pi_extensions_pending
            || self.pi_extensions_mutation_pending
            || self.pi_extension_action_pending
            || !self.pi_extension_update_pending.is_empty()
        {
            return;
        }
        self.pi_extension_action_pending = true;
        self.pi_extensions_generation += 1;
        self.pi_extension_latest_versions.remove(&identity);
        let projects = self.skill_scan_projects();
        let daemon = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let command = if remove {
                waku_client::Command::RemovePiExtension {
                    source: source.clone(),
                    scope,
                    project_root: project_root.clone(),
                    projects: projects.clone(),
                }
            } else {
                waku_client::Command::UpdatePiExtension {
                    source: source.clone(),
                    scope,
                    project_root: project_root.clone(),
                    projects,
                }
            };
            let result = daemon
                .request(Uuid::nil(), Uuid::nil(), command)
                .and_then(|payload| match payload {
                    waku_client::ResponsePayload::Ack => Ok(()),
                    _ => anyhow::bail!("the daemon returned an invalid ack"),
                });
            let _ = this.update(cx, |this, cx| {
                this.pi_extension_action_pending = false;
                this.pi_extension_remove_arming = None;
                match result {
                    Ok(()) => this.show_success_toast(tr!(if remove {
                        "pi_ext.removed_toast"
                    } else {
                        "pi_ext.updated_toast"
                    })),
                    Err(error) => this.show_toast(tr!("pi_ext.action_failed", error = error)),
                }
                this.ensure_pi_extensions(true, cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn remove_pi_extension(
        &mut self,
        source: String,
        scope: PiExtensionScope,
        project_root: Option<PathBuf>,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        if !enabled {
            return;
        }
        let identity = extension_identity_key(&source, scope, project_root.as_deref());
        if self.pi_extension_remove_arming.as_deref() != Some(identity.as_str()) {
            self.pi_extension_remove_arming = Some(identity);
            cx.notify();
            return;
        }
        self.run_pi_extension_action(source, scope, project_root, identity, true, cx);
    }

    /// Read Pi's stable General fields and extension setting values from the
    /// daemon host. This is independent of the package inventory so both
    /// panels can warm in parallel when the page opens.
    pub(super) fn ensure_pi_settings(&mut self, force: bool, cx: &mut Context<Self>) {
        if self.pi_settings_pending || (!force && self.pi_settings.is_some()) {
            return;
        }
        self.pi_settings_pending = true;
        self.pi_settings_generation += 1;
        let generation = self.pi_settings_generation;
        let projects = self.skill_scan_projects();
        let daemon = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    match daemon.request(
                        Uuid::nil(),
                        Uuid::nil(),
                        waku_client::Command::LoadPiSettings { projects },
                    ) {
                        Ok(waku_client::ResponsePayload::PiSettings { snapshot }) => Ok(snapshot),
                        Ok(_) => {
                            anyhow::bail!("the daemon returned an invalid pi settings response")
                        }
                        Err(error) => Err(error),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.pi_settings_generation != generation {
                    return;
                }
                this.pi_settings_pending = false;
                match result {
                    Ok(snapshot) => this.pi_settings = Some(Rc::new(snapshot)),
                    Err(error) => this.show_toast(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn ensure_pi_provider_settings(&mut self, force: bool, cx: &mut Context<Self>) {
        if self.pi_provider_settings_pending || (!force && self.pi_provider_settings.is_some()) {
            return;
        }
        self.pi_provider_settings_pending = true;
        self.pi_provider_settings_generation += 1;
        let generation = self.pi_provider_settings_generation;
        let daemon = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    match daemon.request(
                        Uuid::nil(),
                        Uuid::nil(),
                        waku_client::Command::LoadPiProviderSettings,
                    ) {
                        Ok(waku_client::ResponsePayload::PiProviderSettings { snapshot }) => {
                            Ok(snapshot)
                        }
                        Ok(_) => anyhow::bail!(
                            "the daemon returned an invalid Pi provider settings response"
                        ),
                        Err(error) => Err(error),
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.pi_provider_settings_generation != generation {
                    return;
                }
                this.pi_provider_settings_pending = false;
                match result {
                    Ok(snapshot) => this.pi_provider_settings = Some(Rc::new(snapshot)),
                    Err(error) => this.show_toast(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn clear_pi_provider_form(&mut self, cx: &mut Context<Self>) {
        self.pi_provider_id_input
            .update(cx, |input, _| input.set_read_only(false));
        for input in [
            &self.pi_provider_id_input,
            &self.pi_provider_name_input,
            &self.pi_provider_base_url_input,
            &self.pi_provider_api_input,
            &self.pi_provider_api_key_input,
        ] {
            input.update(cx, |input, cx| input.clear(cx));
        }
        self.pi_provider_form_provider = None;
        self.pi_model_form_model = None;
        self.pi_provider_key_action = PiApiKeyUpdate::Unchanged;
    }

    fn fill_pi_provider_form(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(provider) = self
            .pi_provider_settings
            .as_deref()
            .and_then(|snapshot| snapshot.providers.iter().find(|provider| provider.id == id))
        else {
            return;
        };
        set_pi_input(&self.pi_provider_id_input, provider.id.clone(), cx);
        self.pi_provider_id_input
            .update(cx, |input, _| input.set_read_only(true));
        set_pi_input(
            &self.pi_provider_name_input,
            provider.name.clone().unwrap_or_default(),
            cx,
        );
        set_pi_input(
            &self.pi_provider_base_url_input,
            provider.base_url.clone().unwrap_or_default(),
            cx,
        );
        set_pi_input(
            &self.pi_provider_api_input,
            provider.api.clone().unwrap_or_default(),
            cx,
        );
        self.pi_provider_api_key_input
            .update(cx, |input, cx| input.clear(cx));
        self.pi_provider_key_action = PiApiKeyUpdate::Unchanged;
        self.pi_provider_form_provider = Some(id.to_owned());
    }

    fn save_pi_provider(&mut self, cx: &mut Context<Self>) {
        if self.pi_provider_settings_pending {
            return;
        }
        let id = self
            .pi_provider_form_provider
            .clone()
            .unwrap_or_else(|| pi_input_content(&self.pi_provider_id_input, cx));
        let name = nonempty_pi_input(&self.pi_provider_name_input, cx);
        let base_url = nonempty_pi_input(&self.pi_provider_base_url_input, cx);
        let api = pi_input_content(&self.pi_provider_api_input, cx);
        let key = pi_input_content(&self.pi_provider_api_key_input, cx);
        let api_key = match self.pi_provider_key_action.clone() {
            PiApiKeyUpdate::Clear => PiApiKeyUpdate::Clear,
            PiApiKeyUpdate::Replace(_) => {
                if key.is_empty() {
                    PiApiKeyUpdate::Unchanged
                } else {
                    PiApiKeyUpdate::Replace(key)
                }
            }
            PiApiKeyUpdate::Unchanged => {
                if key.is_empty() {
                    PiApiKeyUpdate::Unchanged
                } else {
                    PiApiKeyUpdate::Replace(key)
                }
            }
        };
        self.pi_provider_settings_pending = true;
        self.pi_provider_settings_generation += 1;
        let generation = self.pi_provider_settings_generation;
        let daemon = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let result = daemon.request(
                Uuid::nil(),
                Uuid::nil(),
                waku_client::Command::UpsertPiProvider {
                    id,
                    name,
                    base_url,
                    api,
                    api_key,
                },
            );
            let _ = this.update(cx, |this, cx| {
                if this.pi_provider_settings_generation != generation {
                    return;
                }
                this.pi_provider_settings_pending = false;
                match result {
                    Ok(waku_client::ResponsePayload::Ack) => {
                        this.clear_pi_provider_form(cx);
                        this.ensure_pi_provider_settings(true, cx);
                    }
                    Ok(_) => this.show_toast("invalid Pi provider response"),
                    Err(error) => this.show_toast(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn delete_pi_provider(&mut self, id: String, cx: &mut Context<Self>) {
        if self.pi_provider_settings_pending {
            return;
        }
        if self.pi_provider_delete_arming.as_deref() != Some(id.as_str()) {
            self.pi_provider_delete_arming = Some(id);
            cx.notify();
            return;
        }
        self.pi_provider_delete_arming = None;
        self.pi_provider_settings_pending = true;
        self.pi_provider_settings_generation += 1;
        let generation = self.pi_provider_settings_generation;
        let daemon = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let result = daemon.request(
                Uuid::nil(),
                Uuid::nil(),
                waku_client::Command::DeletePiProvider { id },
            );
            let _ = this.update(cx, |this, cx| {
                if this.pi_provider_settings_generation != generation {
                    return;
                }
                this.pi_provider_settings_pending = false;
                match result {
                    Ok(waku_client::ResponsePayload::Ack) => {
                        this.clear_pi_provider_form(cx);
                        this.ensure_pi_provider_settings(true, cx);
                    }
                    Ok(_) => this.show_toast("invalid Pi provider response"),
                    Err(error) => this.show_toast(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn fill_new_pi_model_form(&mut self, provider_id: &str, cx: &mut Context<Self>) {
        self.pi_model_provider_input.update(cx, |input, cx| {
            input.set_content(provider_id.to_owned(), cx)
        });
        self.pi_model_provider_input
            .update(cx, |input, _| input.set_read_only(false));
        self.pi_model_id_input
            .update(cx, |input, _| input.set_read_only(false));
        for input in [
            &self.pi_model_id_input,
            &self.pi_model_name_input,
            &self.pi_model_api_input,
            &self.pi_model_reasoning_input,
            &self.pi_model_context_input,
            &self.pi_model_max_tokens_input,
        ] {
            input.update(cx, |input, cx| input.clear(cx));
        }
        self.pi_model_input_input
            .update(cx, |input, cx| input.set_content("text", cx));
        self.pi_model_form_model = None;
        self.pi_model_delete_arming = None;
    }

    fn fill_pi_model_form(&mut self, provider_id: &str, model_id: &str, cx: &mut Context<Self>) {
        let Some(model) = self
            .pi_provider_settings
            .as_deref()
            .and_then(|snapshot| snapshot.providers.iter().find(|p| p.id == provider_id))
            .and_then(|provider| provider.models.iter().find(|model| model.id == model_id))
        else {
            return;
        };
        set_pi_input(&self.pi_model_provider_input, provider_id.to_owned(), cx);
        self.pi_model_provider_input
            .update(cx, |input, _| input.set_read_only(true));
        set_pi_input(&self.pi_model_id_input, model.id.clone(), cx);
        self.pi_model_id_input
            .update(cx, |input, _| input.set_read_only(true));
        set_pi_input(&self.pi_model_name_input, model.name.clone(), cx);
        set_pi_input(
            &self.pi_model_api_input,
            model.api.clone().unwrap_or_default(),
            cx,
        );
        set_pi_input(
            &self.pi_model_reasoning_input,
            model
                .reasoning
                .map(|value| value.to_string())
                .unwrap_or_default(),
            cx,
        );
        set_pi_input(&self.pi_model_input_input, model.input.clone(), cx);
        set_pi_input(
            &self.pi_model_context_input,
            model
                .context_window
                .map(|value| value.to_string())
                .unwrap_or_default(),
            cx,
        );
        set_pi_input(
            &self.pi_model_max_tokens_input,
            model
                .max_tokens
                .map(|value| value.to_string())
                .unwrap_or_default(),
            cx,
        );
        self.pi_model_form_model = Some((provider_id.to_owned(), model_id.to_owned()));
    }

    fn save_pi_model(&mut self, cx: &mut Context<Self>) {
        if self.pi_provider_settings_pending {
            return;
        }
        let (provider_id, model_id) = self.pi_model_form_model.clone().unwrap_or_else(|| {
            (
                pi_input_content(&self.pi_model_provider_input, cx),
                pi_input_content(&self.pi_model_id_input, cx),
            )
        });
        let model = PiModelSnapshot {
            id: model_id,
            name: pi_input_content(&self.pi_model_name_input, cx),
            api: nonempty_pi_input(&self.pi_model_api_input, cx),
            read_only: false,
            reasoning: parse_optional_bool(&self.pi_model_reasoning_input, cx),
            input: {
                let input = pi_input_content(&self.pi_model_input_input, cx);
                if input.is_empty() {
                    "text".into()
                } else {
                    input
                }
            },
            context_window: parse_optional_u64(&self.pi_model_context_input, cx),
            max_tokens: parse_optional_u64(&self.pi_model_max_tokens_input, cx),
        };
        self.pi_provider_settings_pending = true;
        self.pi_provider_settings_generation += 1;
        let generation = self.pi_provider_settings_generation;
        let daemon = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let result = daemon.request(
                Uuid::nil(),
                Uuid::nil(),
                waku_client::Command::UpsertPiModel { provider_id, model },
            );
            let _ = this.update(cx, |this, cx| {
                if this.pi_provider_settings_generation != generation {
                    return;
                }
                this.pi_provider_settings_pending = false;
                match result {
                    Ok(waku_client::ResponsePayload::Ack) => {
                        this.ensure_pi_provider_settings(true, cx);
                    }
                    Ok(_) => this.show_toast("invalid Pi model response"),
                    Err(error) => this.show_toast(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn delete_pi_model(&mut self, provider_id: String, model_id: String, cx: &mut Context<Self>) {
        if self.pi_provider_settings_pending {
            return;
        }
        let identity = (provider_id.clone(), model_id.clone());
        if self.pi_model_delete_arming.as_ref() != Some(&identity) {
            self.pi_model_delete_arming = Some(identity);
            cx.notify();
            return;
        }
        self.pi_model_delete_arming = None;
        self.pi_provider_settings_pending = true;
        self.pi_provider_settings_generation += 1;
        let generation = self.pi_provider_settings_generation;
        let daemon = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let result = daemon.request(
                Uuid::nil(),
                Uuid::nil(),
                waku_client::Command::DeletePiModel {
                    provider_id,
                    model_id,
                },
            );
            let _ = this.update(cx, |this, cx| {
                if this.pi_provider_settings_generation != generation {
                    return;
                }
                this.pi_provider_settings_pending = false;
                match result {
                    Ok(waku_client::ResponsePayload::Ack) => {
                        this.ensure_pi_provider_settings(true, cx);
                    }
                    Ok(_) => this.show_toast("invalid Pi model response"),
                    Err(error) => this.show_toast(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn set_pi_quiet_startup(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.pi_settings_pending {
            return;
        }
        self.pi_settings_pending = true;
        self.pi_settings_generation += 1;
        let generation = self.pi_settings_generation;
        let daemon = self.daemon.client();
        cx.spawn(async move |this, cx| {
            let result = daemon
                .request(
                    Uuid::nil(),
                    Uuid::nil(),
                    waku_client::Command::SetPiQuietStartup { enabled },
                )
                .and_then(|payload| match payload {
                    waku_client::ResponsePayload::Ack => Ok(()),
                    _ => anyhow::bail!("the daemon returned an invalid ack"),
                });
            let _ = this.update(cx, |this, cx| {
                if this.pi_settings_generation != generation {
                    return;
                }
                this.pi_settings_pending = false;
                match result {
                    Ok(()) => this.ensure_pi_settings(true, cx),
                    Err(error) => this.show_toast(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn reload_pi_runtime(&mut self, cx: &mut Context<Self>) {
        let provider = self.selected_session().map(|session| session.provider);
        let session_id = self.state.selected_session;
        let has_runtime = session_id.is_some_and(|id| self.runtimes.contains_key(&id));
        if !can_reload_pi_runtime(provider, has_runtime) {
            return;
        }
        let session_id = session_id.expect("a reloadable Pi session has an id");
        self.reset_session_runtime(session_id);
        self.show_success_toast(tr!("pi_settings.runtime_reloaded"));
        cx.notify();
    }

    // ── Page ───────────────────────────────────────────────────────────────

    pub(super) fn render_pi_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let sections = div()
            .flex()
            .flex_col()
            .gap(px(15.0))
            .child(pi_section(
                theme,
                tr!("pi_settings.general"),
                self.render_pi_general(theme, cx),
            ))
            .child(pi_section(
                theme,
                tr!("pi_settings.providers"),
                self.render_pi_providers(theme, cx),
            ))
            .child(pi_section(
                theme,
                tr!("pi_settings.extensions"),
                self.render_pi_extensions(theme, cx),
            ))
            .child(pi_section(
                theme,
                tr!("pi_settings.extension_settings"),
                self.render_pi_extension_settings(theme),
            ))
            .child(pi_section(
                theme,
                tr!("pi_settings.advanced"),
                self.render_pi_advanced(theme, cx),
            ));

        div()
            .id("pi-settings-scroll")
            .size_full()
            .overflow_y_scroll()
            .px(px(20.0))
            .pt(px(4.0))
            .child(
                div()
                    .w_full()
                    .max_w(px(CONTENT_MAX_WIDTH))
                    .mx_auto()
                    .child(sections),
            )
            .into_any_element()
    }

    fn render_pi_general(&self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(snapshot) = self.pi_settings.as_deref() else {
            return pi_status(theme, tr!("pi_settings.loading"));
        };
        let global = &snapshot.global;
        let mut rows = div().flex().flex_col();
        rows = rows.child(pi_value_row(
            theme,
            tr!("pi_settings.default_provider"),
            optional_pi_value(global.default_provider.as_deref()),
            false,
        ));
        rows = rows.child(pi_value_row(
            theme,
            tr!("pi_settings.default_model"),
            optional_pi_value(global.default_model.as_deref()),
            false,
        ));
        rows = rows.child(pi_value_row(
            theme,
            tr!("pi_settings.default_thinking_level"),
            optional_pi_value(global.default_thinking_level.as_deref()),
            false,
        ));

        let quiet_startup = global.quiet_startup.unwrap_or(false);
        let quiet_toggle = toggle_switch(
            "pi-quiet-startup-toggle",
            quiet_startup,
            self.pi_settings_pending,
            theme,
            cx,
            move |this, _, cx| this.set_pi_quiet_startup(!quiet_startup, cx),
        );
        rows = rows.child(
            div()
                .py(px(8.0))
                .flex()
                .items_center()
                .gap(px(16.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(
                            div()
                                .text_size(sp(12.5))
                                .text_color(theme.text_secondary)
                                .child(tr!("pi_settings.quiet_startup")),
                        )
                        .child(
                            div()
                                .mt(px(3.0))
                                .text_size(sp(11.5))
                                .line_height(sp(15.0))
                                .text_color(theme.text_tertiary)
                                .child(tr!("pi_settings.quiet_startup_description")),
                        ),
                )
                .child(quiet_toggle),
        );
        rows.into_any_element()
    }

    fn render_pi_providers(&self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(snapshot) = self.pi_provider_settings.as_deref() else {
            return pi_status(theme, tr!("pi_settings.loading"));
        };
        let busy = self.pi_provider_settings_pending;
        let mut body = div().flex().flex_col().gap(px(9.0));
        body = body.child(pi_value_row(
            theme,
            tr!("pi_settings.models_path"),
            snapshot.models_path.display().to_string(),
            false,
        ));
        if let Some(error) = snapshot.error.as_deref() {
            body = body.child(
                div()
                    .text_size(sp(11.5))
                    .line_height(sp(15.0))
                    .text_color(theme.warning)
                    .child(SharedString::from(error.to_owned())),
            );
        }

        let save_provider = pi_action_button(
            "pi-provider-save".into(),
            "icons/check.svg",
            tr!("pi_settings.save_provider"),
            !busy,
            theme,
        )
        .when(!busy, |button| {
            button.on_click(cx.listener(|this, _, _, cx| this.save_pi_provider(cx)))
        });
        let clear_provider = pi_action_button(
            "pi-provider-new".into(),
            "icons/plus.svg",
            tr!("pi_settings.new_provider"),
            !busy,
            theme,
        )
        .when(!busy, |button| {
            button.on_click(cx.listener(|this, _, _, cx| {
                this.clear_pi_provider_form(cx);
                cx.notify();
            }))
        });
        let key_clear = pi_action_button(
            "pi-provider-clear-key".into(),
            "icons/x.svg",
            tr!("pi_settings.clear_api_key"),
            !busy,
            theme,
        )
        .when(!busy, |button| {
            button.on_click(cx.listener(|this, _, _, cx| {
                this.pi_provider_key_action = PiApiKeyUpdate::Clear;
                this.pi_provider_api_key_input
                    .update(cx, |input, cx| input.clear(cx));
                cx.notify();
            }))
        });
        let form = div()
            .flex()
            .flex_wrap()
            .gap(px(6.0))
            .child(TextField::new("pi-provider-id", self.pi_provider_id_input.clone()).w(px(150.0)))
            .child(
                TextField::new("pi-provider-name", self.pi_provider_name_input.clone())
                    .w(px(150.0)),
            )
            .child(
                TextField::new(
                    "pi-provider-base-url",
                    self.pi_provider_base_url_input.clone(),
                )
                .w(px(220.0)),
            )
            .child(
                TextField::new("pi-provider-api", self.pi_provider_api_input.clone()).w(px(190.0)),
            )
            .child(
                TextField::new(
                    "pi-provider-api-key",
                    self.pi_provider_api_key_input.clone(),
                )
                .w(px(220.0)),
            )
            .child(clear_provider)
            .child(key_clear)
            .child(save_provider);
        body = body.child(
            div()
                .flex()
                .flex_col()
                .gap(px(5.0))
                .child(
                    div()
                        .text_size(sp(12.5))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text_secondary)
                        .child(tr!("pi_settings.provider_editor")),
                )
                .child(form)
                .child(
                    div()
                        .text_size(sp(10.5))
                        .text_color(theme.text_tertiary)
                        .child(tr!("pi_settings.supported_apis")),
                )
                .child(
                    div()
                        .text_size(sp(10.5))
                        .text_color(theme.text_tertiary)
                        .child(tr!("pi_settings.provider_key_hint")),
                ),
        );

        if snapshot.providers.is_empty() {
            body = body.child(pi_status(theme, tr!("pi_settings.no_providers")));
        } else {
            let mut providers = div().flex().flex_col();
            for provider in &snapshot.providers {
                let provider_id = provider.id.clone();
                let read_only = provider.read_only;
                let edit = pi_action_button(
                    SharedString::from(format!("pi-provider-edit-{}", provider.id)),
                    "icons/pencil.svg",
                    tr!("pi_settings.edit"),
                    !busy && !read_only,
                    theme,
                )
                .when(!busy && !read_only, |button| {
                    button.on_click(cx.listener(move |this, _, _, cx| {
                        this.fill_pi_provider_form(&provider_id, cx);
                        cx.notify();
                    }))
                });
                let delete_id = provider.id.clone();
                let delete_armed =
                    self.pi_provider_delete_arming.as_deref() == Some(provider.id.as_str());
                let delete = pi_action_button(
                    SharedString::from(format!("pi-provider-delete-{}", provider.id)),
                    "icons/trash.svg",
                    if delete_armed {
                        tr!("pi_settings.confirm_delete")
                    } else {
                        tr!("pi_settings.delete")
                    },
                    !busy && !read_only && provider.models.is_empty(),
                    theme,
                )
                .when(
                    !busy && !read_only && provider.models.is_empty(),
                    |button| {
                        button.on_click(cx.listener(move |this, _, _, cx| {
                            this.delete_pi_provider(delete_id.clone(), cx);
                        }))
                    },
                );
                let mut model_rows = div().flex().flex_col();
                if provider.models.is_empty() {
                    model_rows = model_rows.child(pi_status(theme, tr!("pi_settings.no_models")));
                } else {
                    for model in &provider.models {
                        let model_id = model.id.clone();
                        let provider_for_model = provider.id.clone();
                        let model_read_only = read_only || model.read_only;
                        let edit_model = pi_action_button(
                            SharedString::from(format!(
                                "pi-model-edit-{}-{}",
                                provider.id, model.id
                            )),
                            "icons/pencil.svg",
                            tr!("pi_settings.edit"),
                            !busy && !model_read_only,
                            theme,
                        )
                        .when(!busy && !model_read_only, |button| {
                            button.on_click(cx.listener(move |this, _, _, cx| {
                                this.fill_pi_model_form(&provider_for_model, &model_id, cx);
                                cx.notify();
                            }))
                        });
                        let delete_model_provider = provider.id.clone();
                        let delete_model_id = model.id.clone();
                        let model_delete_armed = self.pi_model_delete_arming.as_ref()
                            == Some(&(provider.id.clone(), model.id.clone()));
                        let delete_model = pi_action_button(
                            SharedString::from(format!(
                                "pi-model-delete-{}-{}",
                                provider.id, model.id
                            )),
                            "icons/trash.svg",
                            if model_delete_armed {
                                tr!("pi_settings.confirm_delete")
                            } else {
                                tr!("pi_settings.delete")
                            },
                            !busy && !model_read_only,
                            theme,
                        )
                        .when(!busy && !model_read_only, |button| {
                            button.on_click(cx.listener(move |this, _, _, cx| {
                                this.delete_pi_model(
                                    delete_model_provider.clone(),
                                    delete_model_id.clone(),
                                    cx,
                                );
                            }))
                        });
                        model_rows = model_rows.child(
                            div()
                                .py(px(5.0))
                                .border_t_1()
                                .border_color(theme.border)
                                .flex()
                                .items_center()
                                .gap(px(7.0))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .font_family(crate::md::render::MONO_FAMILY)
                                        .text_size(sp(11.5))
                                        .text_color(theme.text_secondary)
                                        .child(SharedString::from(format!(
                                            "{} · {}",
                                            model.id, model.name
                                        ))),
                                )
                                .child(edit_model)
                                .child(delete_model),
                        );
                    }
                }
                let add_provider = provider.id.clone();
                let add_model = pi_action_button(
                    SharedString::from(format!("pi-model-new-{}", provider.id)),
                    "icons/plus.svg",
                    tr!("pi_settings.new_model"),
                    !busy && !read_only,
                    theme,
                )
                .when(!busy && !read_only, |button| {
                    button.on_click(cx.listener(move |this, _, _, cx| {
                        this.fill_new_pi_model_form(&add_provider, cx);
                        cx.notify();
                    }))
                });
                let mut title = div().flex().items_center().gap(px(7.0)).child(
                    div()
                        .text_size(sp(13.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(SharedString::from(provider.id.clone())),
                );
                if let Some(api) = provider.api.as_deref() {
                    title = title.child(
                        div()
                            .font_family(crate::md::render::MONO_FAMILY)
                            .text_size(sp(10.5))
                            .text_color(theme.text_tertiary)
                            .child(SharedString::from(api.to_owned())),
                    );
                }
                title = title.child(div().flex_1()).child(edit).child(delete);
                let mut card = div()
                    .py(px(7.0))
                    .border_t_1()
                    .border_color(theme.border)
                    .child(title)
                    .child(
                        div()
                            .text_size(sp(11.5))
                            .text_color(theme.text_tertiary)
                            .child(SharedString::from(format!(
                                "{} · {}",
                                if provider.api_key_configured {
                                    tr!("pi_settings.api_key_configured")
                                } else {
                                    tr!("pi_settings.api_key_not_configured")
                                },
                                provider.name.as_deref().unwrap_or("—")
                            ))),
                    )
                    .child(model_rows)
                    .child(add_model);
                if read_only {
                    card = card.child(
                        div()
                            .text_size(sp(10.5))
                            .text_color(theme.text_tertiary)
                            .child(tr!("pi_settings.read_only_provider")),
                    );
                }
                providers = providers.child(card);
            }
            body = body.child(providers);
        }

        let save_model = pi_action_button(
            "pi-model-save".into(),
            "icons/check.svg",
            tr!("pi_settings.save_model"),
            !busy,
            theme,
        )
        .when(!busy, |button| {
            button.on_click(cx.listener(|this, _, _, cx| this.save_pi_model(cx)))
        });
        body = body.child(
            div()
                .pt(px(8.0))
                .border_t_1()
                .border_color(theme.border)
                .flex()
                .flex_wrap()
                .gap(px(6.0))
                .child(
                    TextField::new("pi-model-provider", self.pi_model_provider_input.clone())
                        .w(px(140.0)),
                )
                .child(TextField::new("pi-model-id", self.pi_model_id_input.clone()).w(px(150.0)))
                .child(
                    TextField::new("pi-model-name", self.pi_model_name_input.clone()).w(px(150.0)),
                )
                .child(TextField::new("pi-model-api", self.pi_model_api_input.clone()).w(px(190.0)))
                .child(
                    TextField::new("pi-model-reasoning", self.pi_model_reasoning_input.clone())
                        .w(px(160.0)),
                )
                .child(
                    TextField::new("pi-model-input", self.pi_model_input_input.clone())
                        .w(px(170.0)),
                )
                .child(
                    TextField::new("pi-model-context", self.pi_model_context_input.clone())
                        .w(px(140.0)),
                )
                .child(
                    TextField::new(
                        "pi-model-max-tokens",
                        self.pi_model_max_tokens_input.clone(),
                    )
                    .w(px(140.0)),
                )
                .child(save_model),
        );
        body.into_any_element()
    }

    fn render_pi_extensions(&self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut rows = div().flex().flex_col();
        match self.pi_extensions.as_deref() {
            None => rows = rows.child(pi_status(theme, tr!("pi_ext.loading"))),
            Some(extensions) if extensions.is_empty() => {
                rows = rows.child(pi_status(theme, tr!("pi_settings.no_extensions")))
            }
            Some(extensions) => {
                for extension in extensions {
                    let compat = compatibility(&extension.source);
                    let enabled = extension.enabled;
                    let manageable = extension.manageable;
                    let identity = extension_identity_key(
                        &extension.source,
                        extension.scope,
                        extension.project_root.as_deref(),
                    );
                    let inventory_busy = self.pi_extensions_pending
                        || self.pi_extensions_mutation_pending
                        || self.pi_extension_action_pending
                        || !self.pi_extension_update_pending.is_empty();
                    let update_check_pending = self.pi_extension_update_pending.contains(&identity);
                    let latest_version = self
                        .pi_extension_latest_versions
                        .get(&identity)
                        .and_then(|version| version.as_deref());
                    let has_newer_version =
                        is_newer_pi_extension_version(extension.version.as_deref(), latest_version);
                    // Owned clones for the 'static toggle closure.
                    let toggle_source = extension.source.clone();
                    let toggle_scope = extension.scope;
                    let toggle_project_root = extension.project_root.clone();
                    let scope_label = match extension.scope {
                        PiExtensionScope::User => tr!("pi_ext.scope_user"),
                        PiExtensionScope::Project => tr!("pi_ext.scope_project"),
                    };
                    let toggle = toggle_switch(
                        SharedString::from(extension_toggle_id(
                            &extension.source,
                            extension.scope,
                            extension.project_root.as_deref(),
                        )),
                        enabled,
                        inventory_busy || !manageable,
                        theme,
                        cx,
                        move |this, _, cx| {
                            this.set_pi_extension_enabled(
                                toggle_source.clone(),
                                toggle_scope,
                                toggle_project_root.clone(),
                                !enabled,
                                cx,
                            );
                        },
                    );

                    let check_enabled = manageable
                        && can_check_pi_extension(&extension.source)
                        && !inventory_busy
                        && !update_check_pending;
                    let check_source = extension.source.clone();
                    let check_identity = identity.clone();
                    let check = pi_action_button(
                        SharedString::from(format!("pi-ext-check-{identity}")),
                        "icons/rotate-cw.svg",
                        if update_check_pending {
                            tr!("pi_ext.checking_update")
                        } else {
                            tr!("pi_ext.check_update")
                        },
                        check_enabled,
                        theme,
                    )
                    .when(check_enabled, |button| {
                        button.on_click(cx.listener(move |this, _, _, cx| {
                            this.check_pi_extension_update(
                                check_source.clone(),
                                check_identity.clone(),
                                cx,
                            );
                        }))
                    });

                    let update_source = extension.source.clone();
                    let update_scope = extension.scope;
                    let update_project_root = extension.project_root.clone();
                    let update_identity = identity.clone();
                    let update = pi_action_button(
                        SharedString::from(format!("pi-ext-update-{identity}")),
                        "icons/download.svg",
                        tr!("pi_ext.update"),
                        manageable && has_newer_version && !inventory_busy,
                        theme,
                    )
                    .when(
                        manageable && has_newer_version && !inventory_busy,
                        |button| {
                            button.on_click(cx.listener(move |this, _, _, cx| {
                                this.run_pi_extension_action(
                                    update_source.clone(),
                                    update_scope,
                                    update_project_root.clone(),
                                    update_identity.clone(),
                                    false,
                                    cx,
                                );
                            }))
                        },
                    );

                    let remove_armed =
                        self.pi_extension_remove_arming.as_deref() == Some(identity.as_str());
                    let remove_enabled = enabled && manageable && !inventory_busy;
                    let remove_source = extension.source.clone();
                    let remove_scope = extension.scope;
                    let remove_project_root = extension.project_root.clone();
                    let remove_identity = identity.clone();
                    let remove = pi_action_button(
                        SharedString::from(format!("pi-ext-remove-{identity}")),
                        "icons/trash.svg",
                        if remove_armed {
                            tr!("pi_ext.confirm_remove")
                        } else {
                            tr!("pi_ext.remove")
                        },
                        remove_enabled,
                        theme,
                    )
                    .when(remove_enabled, |button| {
                        button.on_click(cx.listener(move |this, _, _, cx| {
                            this.remove_pi_extension(
                                remove_source.clone(),
                                remove_scope,
                                remove_project_root.clone(),
                                enabled,
                                cx,
                            );
                        }))
                    })
                    .on_mouse_down_out(cx.listener(move |this, _, _, cx| {
                        if this.pi_extension_remove_arming.as_deref()
                            == Some(remove_identity.as_str())
                        {
                            this.pi_extension_remove_arming = None;
                            cx.notify();
                        }
                    }));
                    let actions = div()
                        .flex()
                        .items_center()
                        .gap(px(4.0))
                        .child(check)
                        .child(update)
                        .child(remove);
                    let mut headline = div().flex().items_baseline().gap(px(6.0)).child(
                        div()
                            .text_size(sp(13.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(extension.name.clone())),
                    );
                    if let Some(version) = extension.version.as_deref() {
                        headline = headline.child(
                            div()
                                .text_size(sp(11.5))
                                .font_family(crate::md::render::MONO_FAMILY)
                                .text_color(theme.text_tertiary)
                                .child(SharedString::from(version.to_owned())),
                        );
                    }
                    let mut badges = div().flex().items_center().gap(px(5.0)).child(
                        div()
                            .h(px(17.0))
                            .px(px(6.0))
                            .rounded(px(5.0))
                            .bg(theme.overlay)
                            .flex()
                            .items_center()
                            .text_size(sp(11.0))
                            .text_color(theme.text_tertiary)
                            .child(scope_label),
                    );
                    badges = badges.child(
                        div()
                            .h(px(17.0))
                            .px(px(6.0))
                            .rounded(px(5.0))
                            .when(compat.emphasized(), |chip| chip.bg(theme.code_wash))
                            .flex()
                            .items_center()
                            .text_size(sp(11.0))
                            .when(compat.emphasized(), |chip| {
                                chip.text_color(theme.code_text)
                                    .font_family(crate::md::render::MONO_FAMILY)
                            })
                            .when(!compat.emphasized(), |chip| {
                                chip.text_color(theme.text_tertiary)
                            })
                            .child(compat.label()),
                    );

                    rows = rows.child(
                        div()
                            .w_full()
                            .py(px(9.0))
                            .border_b_1()
                            .border_color(theme.border)
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.0))
                                    .child(headline)
                                    .children(extension.description.as_ref().map(|description| {
                                        div()
                                            .text_size(sp(11.5))
                                            .line_height(sp(14.0))
                                            .text_color(theme.text_tertiary)
                                            .truncate()
                                            .child(SharedString::from(description.clone()))
                                    }))
                                    .child(badges)
                                    .children(if !manageable {
                                        Some(
                                            div()
                                                .text_size(sp(10.5))
                                                .text_color(theme.text_tertiary)
                                                .child(tr!("pi_ext.discovered_read_only")),
                                        )
                                    } else if !enabled {
                                        Some(
                                            div()
                                                .text_size(sp(10.5))
                                                .text_color(theme.warning)
                                                .child(tr!("pi_ext.enable_before_remove")),
                                        )
                                    } else {
                                        None
                                    })
                                    .children(
                                        (update_check_pending
                                            || self
                                                .pi_extension_latest_versions
                                                .contains_key(&identity))
                                        .then(|| {
                                            let status = if update_check_pending {
                                                tr!("pi_ext.checking_update")
                                            } else if latest_version.is_some() && has_newer_version
                                            {
                                                tr!("pi_ext.update_available")
                                            } else if latest_version.is_some()
                                                && extension.version.as_deref().is_some_and(
                                                    |version| parse_pi_semver(version).is_some(),
                                                )
                                            {
                                                tr!("pi_ext.up_to_date")
                                            } else {
                                                tr!("pi_ext.update_unavailable")
                                            };
                                            div()
                                                .text_size(sp(10.5))
                                                .text_color(if has_newer_version {
                                                    theme.accent
                                                } else {
                                                    theme.text_tertiary
                                                })
                                                .child(status)
                                        }),
                                    ),
                            )
                            .child(actions)
                            .child(toggle),
                    );
                }
            }
        }

        div()
            .child(
                div().flex().items_center().gap(px(6.0)).pb(px(4.0)).child(
                    div()
                        .text_size(sp(11.5))
                        .text_color(theme.text_tertiary)
                        .child(tr!("pi_ext.restart_note")),
                ),
            )
            .child(
                div()
                    .w_full()
                    .rounded(px(9.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.composer)
                    .px(px(14.0))
                    .py(px(4.0))
                    .child(rows),
            )
            .into_any_element()
    }

    fn render_pi_extension_settings(&self, theme: Theme) -> AnyElement {
        let Some(snapshot) = self.pi_settings.as_deref() else {
            return pi_status(theme, tr!("pi_settings.loading"));
        };
        let mut scopes = div().flex().flex_col().gap(px(10.0));
        scopes = scopes.child(render_pi_scope_settings(
            theme,
            tr!("pi_settings.global"),
            &snapshot.global,
        ));
        if snapshot.projects.is_empty() {
            scopes = scopes.child(pi_status(theme, tr!("pi_settings.no_project_settings")));
        } else {
            for project in &snapshot.projects {
                scopes = scopes.child(render_pi_scope_settings(
                    theme,
                    format!("{} · {}", tr!("pi_settings.project"), project.name),
                    &project.settings,
                ));
            }
        }
        scopes.into_any_element()
    }

    fn render_pi_advanced(&self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        let global_path = self
            .pi_settings
            .as_deref()
            .map(|snapshot| snapshot.global.config_path.clone());
        let mut body = div().flex().flex_col().gap(px(9.0));
        body = body.child(pi_value_row(
            theme,
            tr!("pi_settings.config_path"),
            global_path
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| tr!("pi_settings.loading")),
            true,
        ));

        if self.daemon.is_remote() {
            body = body.child(
                div()
                    .text_size(sp(11.5))
                    .line_height(sp(15.0))
                    .text_color(theme.text_tertiary)
                    .child(tr!("pi_settings.remote_unavailable")),
            );
        } else if let Some(path) = global_path {
            let action_button = |id: SharedString, icon_path: &'static str, label: String| {
                div()
                    .id(id)
                    .tab_index(0)
                    .h(px(27.0))
                    .px(px(10.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(theme.border_strong)
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .cursor_default()
                    .text_size(sp(12.5))
                    .text_color(theme.text_secondary)
                    .focus_visible(|style| style.border_color(theme.accent))
                    .hover(|element| element.bg(theme.overlay))
                    .child(icon(icon_path, 11.0, theme.text_tertiary))
                    .child(SharedString::from(label))
            };
            let open_path = path.clone();
            let open = action_button(
                "pi-open-config".into(),
                "icons/pencil.svg",
                tr!("pi_settings.open_config"),
            )
            .on_click(cx.listener(move |_, _, _, cx| {
                crate::platform::open_with_default_app(&open_path, cx);
            }));
            let reveal_path = path.clone();
            let reveal = action_button(
                "pi-reveal-config".into(),
                "icons/folder.svg",
                tr!("pi_settings.reveal_config"),
            )
            .on_click(cx.listener(move |_, _, _, cx| {
                crate::platform::reveal_in_file_manager(&reveal_path, cx);
            }));
            body = body.child(div().flex().gap(px(7.0)).child(open).child(reveal));
        }

        let provider = self.selected_session().map(|session| session.provider);
        let session_id = self.state.selected_session;
        let has_runtime = session_id.is_some_and(|id| self.runtimes.contains_key(&id));
        let can_reload = can_reload_pi_runtime(provider, has_runtime);
        let reload = div()
            .id("pi-reload-runtime")
            .tab_index(0)
            .h(px(28.0))
            .px(px(11.0))
            .rounded(px(7.0))
            .border_1()
            .border_color(theme.border_strong)
            .flex()
            .items_center()
            .gap(px(6.0))
            .cursor_default()
            .text_size(sp(12.5))
            .text_color(theme.text_secondary)
            .opacity(if can_reload { 1.0 } else { 0.55 })
            .focus_visible(|style| style.border_color(theme.accent))
            .when(can_reload, |element| {
                element
                    .hover(|element| element.bg(theme.overlay))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reload_pi_runtime(cx);
                    }))
            })
            .child(icon("icons/rotate-cw.svg", 11.0, theme.text_tertiary))
            .child(tr!("pi_settings.reload_runtime"));
        body = body.child(
            div()
                .pt(px(4.0))
                .border_t_1()
                .border_color(theme.border)
                .child(
                    div()
                        .text_size(sp(11.5))
                        .line_height(sp(15.0))
                        .text_color(theme.text_tertiary)
                        .child(tr!("pi_settings.reload_runtime_description")),
                )
                .child(reload),
        );
        body.into_any_element()
    }
}

fn set_pi_input(input: &Entity<TextInput>, value: String, cx: &mut Context<Waku>) {
    input.update(cx, |input, cx| input.set_content(value, cx));
}

fn pi_input_content(input: &Entity<TextInput>, cx: &Context<Waku>) -> String {
    input.read(cx).content().to_owned()
}

fn nonempty_pi_input(input: &Entity<TextInput>, cx: &Context<Waku>) -> Option<String> {
    let value = pi_input_content(input, cx);
    (!value.trim().is_empty()).then_some(value)
}

fn parse_optional_bool(input: &Entity<TextInput>, cx: &Context<Waku>) -> Option<bool> {
    match pi_input_content(input, cx).trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn parse_optional_u64(input: &Entity<TextInput>, cx: &Context<Waku>) -> Option<u64> {
    pi_input_content(input, cx).trim().parse().ok()
}

fn pi_section(theme: Theme, title: String, body: AnyElement) -> Div {
    div()
        .w_full()
        .rounded(px(11.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.raised)
        .px(px(14.0))
        .py(px(10.0))
        .child(
            div()
                .pb(px(5.0))
                .text_size(sp(13.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.text)
                .child(SharedString::from(title)),
        )
        .child(body)
}

fn pi_status(theme: Theme, message: String) -> AnyElement {
    div()
        .py(px(9.0))
        .text_size(sp(12.5))
        .text_color(theme.text_tertiary)
        .child(SharedString::from(message))
        .into_any_element()
}

fn pi_action_button(
    id: SharedString,
    icon_path: &'static str,
    label: String,
    enabled: bool,
    theme: Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .tab_index(0)
        .h(px(24.0))
        .px(px(7.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(theme.border_strong)
        .flex()
        .items_center()
        .gap(px(4.0))
        .cursor_default()
        .text_size(sp(11.0))
        .text_color(theme.text_secondary)
        .opacity(if enabled { 1.0 } else { 0.5 })
        .focus_visible(|style| style.border_color(theme.accent))
        .when(enabled, |element| {
            element.hover(|element| element.bg(theme.overlay))
        })
        .child(icon(icon_path, 10.0, theme.text_tertiary))
        .child(SharedString::from(label))
}

fn pi_value_row(theme: Theme, label: String, value: String, last: bool) -> Div {
    div()
        .py(px(7.0))
        .when(!last, |element| {
            element.border_b_1().border_color(theme.border)
        })
        .flex()
        .items_baseline()
        .gap(px(12.0))
        .child(
            div()
                .w(px(140.0))
                .flex_none()
                .text_size(sp(12.5))
                .text_color(theme.text_tertiary)
                .child(SharedString::from(label)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(crate::md::render::MONO_FAMILY)
                .text_size(sp(11.5))
                .text_color(theme.text_secondary)
                .child(SharedString::from(value)),
        )
}

fn render_pi_scope_settings(theme: Theme, label: String, scope: &PiSettingsScopeSnapshot) -> Div {
    let mut rows = div().flex().flex_col();
    if let Some(error) = scope.error.as_deref() {
        rows = rows.child(
            div()
                .py(px(7.0))
                .text_size(sp(11.5))
                .line_height(sp(15.0))
                .text_color(theme.warning)
                .child(SharedString::from(error.to_owned())),
        );
    }
    if scope.extension_settings.is_empty() {
        rows = rows.child(pi_status(theme, tr!("pi_settings.no_extension_settings")));
    } else {
        for group in &scope.extension_settings {
            rows = rows.child(render_pi_settings_group(theme, group));
        }
    }
    div()
        .w_full()
        .rounded(px(8.0))
        .border_1()
        .border_color(theme.border)
        .bg(theme.composer)
        .px(px(11.0))
        .child(
            div()
                .py(px(7.0))
                .text_size(sp(12.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.text_secondary)
                .child(SharedString::from(label)),
        )
        .child(rows)
}

fn render_pi_settings_group(theme: Theme, group: &PiExtensionSettingsGroup) -> Div {
    let mut entries = div().flex().flex_col();
    for (index, entry) in group.entries.iter().enumerate() {
        entries = entries.child(
            div()
                .py(px(6.0))
                .when(index > 0, |element| {
                    element.border_t_1().border_color(theme.border)
                })
                .flex()
                .items_baseline()
                .gap(px(10.0))
                .child(
                    div()
                        .w(px(140.0))
                        .flex_none()
                        .truncate()
                        .font_family(crate::md::render::MONO_FAMILY)
                        .text_size(sp(11.5))
                        .text_color(theme.text_tertiary)
                        .child(SharedString::from(entry.key.clone())),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(sp(11.5))
                        .text_color(theme.text_secondary)
                        .child(SharedString::from(compact_pi_setting_value(&entry.value))),
                ),
        );
    }
    div()
        .pt(px(4.0))
        .child(
            div()
                .text_size(sp(11.5))
                .font_family(crate::md::render::MONO_FAMILY)
                .text_color(theme.text_secondary)
                .child(SharedString::from(group.extension.clone())),
        )
        .child(entries)
}

fn optional_pi_value(value: Option<&str>) -> String {
    value
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| tr!("pi_settings.not_set"))
}

fn compact_pi_setting_value(value: &Value) -> String {
    let rendered = match value {
        Value::String(value) => value.clone(),
        _ => serde_json::to_string(value).unwrap_or_else(|_| tr!("pi_settings.unavailable")),
    };
    let mut chars = rendered.chars();
    let visible = chars
        .by_ref()
        .take(PI_SETTING_VALUE_MAX_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        format!("{visible}…")
    } else {
        visible
    }
}

fn can_reload_pi_runtime(provider: Option<ProviderKind>, has_runtime: bool) -> bool {
    provider == Some(ProviderKind::Pi) && has_runtime
}

fn can_check_pi_extension(source: &str) -> bool {
    source
        .strip_prefix("npm:")
        .is_some_and(|package| !package.is_empty() && !package.chars().any(char::is_whitespace))
}

fn extension_toggle_id(
    source: &str,
    scope: PiExtensionScope,
    project_root: Option<&std::path::Path>,
) -> String {
    format!(
        "pi-ext-{}",
        extension_identity_key(source, scope, project_root)
    )
}

fn extension_identity_key(
    source: &str,
    scope: PiExtensionScope,
    project_root: Option<&std::path::Path>,
) -> String {
    let scope = match scope {
        PiExtensionScope::User => "user",
        PiExtensionScope::Project => "project",
    };
    let project_root = project_root
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "global".to_owned());
    format!(
        "{}-{}-{}",
        element_id_component(source),
        scope,
        element_id_component(&project_root)
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PiSemver {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: Vec<PiSemverIdentifier>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PiSemverIdentifier {
    Numeric(u64),
    Text(String),
}

fn parse_pi_semver(value: &str) -> Option<PiSemver> {
    let value = value.trim();
    let (without_build, build) = value
        .split_once('+')
        .map_or((value, None), |(core, build)| (core, Some(build)));
    if build.is_some_and(|build| {
        build.is_empty()
            || !build.split('.').all(|identifier| {
                !identifier.is_empty()
                    && identifier
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric() || character == '-')
            })
    }) {
        return None;
    }
    let (core, prerelease) = without_build
        .split_once('-')
        .map_or((without_build, None), |(core, prerelease)| {
            (core, Some(prerelease))
        });
    let mut components = core.split('.');
    let parse_number = |component: &str| {
        (!component.is_empty()
            && component
                .chars()
                .all(|character| character.is_ascii_digit())
            && (component == "0" || !component.starts_with('0')))
        .then(|| component.parse().ok())
        .flatten()
    };
    let major = parse_number(components.next()?)?;
    let minor = parse_number(components.next()?)?;
    let patch = parse_number(components.next()?)?;
    if components.next().is_some() {
        return None;
    }
    let prerelease = prerelease
        .map(|value| {
            value
                .split('.')
                .map(|identifier| {
                    if identifier.is_empty()
                        || !identifier
                            .chars()
                            .all(|character| character.is_ascii_alphanumeric() || character == '-')
                    {
                        return None;
                    }
                    if identifier
                        .chars()
                        .all(|character| character.is_ascii_digit())
                    {
                        if identifier != "0" && identifier.starts_with('0') {
                            return None;
                        }
                        Some(PiSemverIdentifier::Numeric(identifier.parse().ok()?))
                    } else {
                        Some(PiSemverIdentifier::Text(identifier.to_owned()))
                    }
                })
                .collect::<Option<Vec<_>>>()
        })
        .unwrap_or_else(|| Some(Vec::new()))?;
    Some(PiSemver {
        major,
        minor,
        patch,
        prerelease,
    })
}

impl Ord for PiSemverIdentifier {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Numeric(left), Self::Numeric(right)) => left.cmp(right),
            (Self::Numeric(_), Self::Text(_)) => Ordering::Less,
            (Self::Text(_), Self::Numeric(_)) => Ordering::Greater,
            (Self::Text(left), Self::Text(right)) => left.cmp(right),
        }
    }
}

impl PartialOrd for PiSemverIdentifier {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PiSemver {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(
                || match (self.prerelease.is_empty(), other.prerelease.is_empty()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) => self
                        .prerelease
                        .iter()
                        .zip(&other.prerelease)
                        .map(|(left, right)| left.cmp(right))
                        .find(|ordering| *ordering != Ordering::Equal)
                        .unwrap_or_else(|| self.prerelease.len().cmp(&other.prerelease.len())),
                },
            )
    }
}

impl PartialOrd for PiSemver {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn is_newer_pi_extension_version(current: Option<&str>, latest: Option<&str>) -> bool {
    let (Some(current), Some(latest)) = (current, latest) else {
        return false;
    };
    match (parse_pi_semver(current), parse_pi_semver(latest)) {
        (Some(current), Some(latest)) => latest > current,
        _ => false,
    }
}

fn element_id_component(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_setting_values_are_compact_and_bounded() {
        assert_eq!(
            compact_pi_setting_value(&Value::String("fast mode".to_owned())),
            "fast mode"
        );
        assert_eq!(
            compact_pi_setting_value(&serde_json::json!({"enabled": true, "items": [1, 2]})),
            r#"{"enabled":true,"items":[1,2]}"#
        );
        let long =
            compact_pi_setting_value(&Value::String("x".repeat(PI_SETTING_VALUE_MAX_CHARS + 4)));
        assert_eq!(long.chars().count(), PI_SETTING_VALUE_MAX_CHARS + 1);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn runtime_reload_is_limited_to_a_live_pi_session() {
        assert!(can_reload_pi_runtime(Some(ProviderKind::Pi), true));
        assert!(!can_reload_pi_runtime(Some(ProviderKind::Pi), false));
        assert!(!can_reload_pi_runtime(Some(ProviderKind::OhMyPi), true));
        assert!(!can_reload_pi_runtime(None, true));
    }

    #[test]
    fn extension_toggle_ids_include_scope_and_project_root() {
        let project = std::path::Path::new("/workspace/project");
        assert_ne!(
            extension_toggle_id("npm:demo", PiExtensionScope::User, None),
            extension_toggle_id("npm:demo", PiExtensionScope::Project, Some(project))
        );
        assert_ne!(
            extension_toggle_id("npm:demo", PiExtensionScope::Project, Some(project)),
            extension_toggle_id(
                "npm:demo",
                PiExtensionScope::Project,
                Some(std::path::Path::new("/workspace/other"))
            )
        );
    }

    #[test]
    fn element_id_components_are_collision_free_for_paths() {
        let with_space = element_id_component("/tmp/a b");
        let with_question = element_id_component("/tmp/a?b");
        assert_eq!(with_space, "%2Ftmp%2Fa%20b");
        assert_eq!(with_question, "%2Ftmp%2Fa%3Fb");
        assert_ne!(with_space, with_question);
    }

    #[test]
    fn update_requires_a_valid_newer_semver() {
        assert!(is_newer_pi_extension_version(Some("1.2.3"), Some("1.2.4")));
        assert!(!is_newer_pi_extension_version(Some("1.2.3"), Some("1.2.3")));
        assert!(is_newer_pi_extension_version(
            Some("2.0.0-beta.1"),
            Some("2.0.0-beta.2")
        ));
        assert!(is_newer_pi_extension_version(
            Some("2.0.0-beta"),
            Some("2.0.0")
        ));
        assert!(!is_newer_pi_extension_version(
            Some("not-a-version"),
            Some("2.0.0")
        ));
    }

    #[test]
    fn update_checks_are_limited_to_valid_npm_sources() {
        assert!(can_check_pi_extension("npm:pi-demo"));
        assert!(!can_check_pi_extension("npm:"));
        assert!(!can_check_pi_extension("npm:@scope/pi demo"));
        assert!(!can_check_pi_extension("github:owner/pi-demo"));
    }
}

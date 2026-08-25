//! PIWAKU: the Settings → Pi page — the installed pi extensions manager.
//!
//! The inventory lives on the daemon host (a remote client's `~/.pi` is
//! irrelevant); the page only renders what [`Command::LoadPiExtensions`]
//! returned and flips packages through [`Command::SetPiExtensionEnabled`].
//! Compatibility badges are Piwaku's own hardcoded metadata — the installed
//! state always comes from the daemon.

use super::*;
use crate::model::PiExtensionScope;

/// How long a cached pi extension inventory stays trusted.
const PI_EXTENSIONS_RESCAN_AFTER: std::time::Duration = std::time::Duration::from_secs(15);

/// PIWAKU: temporary file trace for the inventory load path — the app's
/// stderr is detached under Launch Services, so terminal prints never land.
fn trace_pi_ext(message: &str) {
    use std::io::Write as _;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/piwaku-pi-ext.log")
    {
        let _ = writeln!(file, "{message}");
    }
}

/// Sources with a dedicated Piwaku adapter or a deliberate non-adapter
/// stance. Anything absent renders as generic-compatible.
fn compatibility(source: &str) -> PiCompatibility {
    match source {
        "npm:@juicesharp/rpiv-ask-user-question" | "npm:@juicesharp/rpiv-todo" => {
            PiCompatibility::Native
        }
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
        let projects = self.skill_scan_projects();
        let daemon = self.daemon.client();
        trace_pi_ext(&format!(
            "[pi-ext] requesting inventory ({} projects)",
            projects.len()
        ));
        cx.spawn(async move |this, cx| {
            trace_pi_ext("[pi-ext] outer task started");
            let extensions = cx
                .background_executor()
                .spawn(async move {
                    trace_pi_ext("[pi-ext] inner task running — sending request");
                    match daemon.request(
                        Uuid::nil(),
                        Uuid::nil(),
                        waku_client::Command::LoadPiExtensions { projects },
                    ) {
                        Ok(waku_client::ResponsePayload::PiExtensions { extensions }) => {
                            trace_pi_ext(&format!(
                                "[pi-ext] inventory arrived: {} packages",
                                extensions.len()
                            ));
                            Ok(extensions)
                        }
                        Ok(_) => {
                            trace_pi_ext("[pi-ext] daemon returned an unexpected payload");
                            anyhow::bail!("the daemon returned an invalid pi extensions response")
                        }
                        Err(error) => {
                            trace_pi_ext(&format!("[pi-ext] request failed: {error:#}"));
                            Err(error)
                        }
                    }
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                trace_pi_ext(&format!(
                    "[pi-ext] applying result (generation {}/{})",
                    this.pi_extensions_generation, generation
                ));
                if this.pi_extensions_generation != generation {
                    return;
                }
                this.pi_extensions_pending = false;
                match extensions {
                    Ok(extensions) => {
                        this.pi_extensions = Some(Rc::new(extensions));
                        this.pi_extensions_scanned_at = Some(Instant::now());
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
                this.ensure_pi_extensions(true, cx);
                cx.notify();
            });
        })
        .detach();
    }

    // ── Page ───────────────────────────────────────────────────────────────

    pub(super) fn render_pi_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::current(cx);
        let Some(extensions) = self.pi_extensions.clone() else {
            return div()
                .size_full()
                .pt(px(12.0))
                .child(
                    div()
                        .px(px(20.0))
                        .text_size(sp(12.5))
                        .text_color(theme.text_tertiary)
                        .child(tr!("pi_ext.loading")),
                )
                .into_any_element();
        };

        let mut rows = div().flex().flex_col();
        for extension in extensions.iter() {
            let compat = compatibility(&extension.source);
            let enabled = extension.enabled;
            // Owned clones for the 'static toggle closure.
            let toggle_source = extension.source.clone();
            let toggle_scope = extension.scope;
            let toggle_project_root = extension.project_root.clone();
            let scope_label = match extension.scope {
                PiExtensionScope::User => tr!("pi_ext.scope_user"),
                PiExtensionScope::Project => tr!("pi_ext.scope_project"),
            };
            let toggle = toggle_switch(
                SharedString::from(format!("pi-ext-{}", extension.source)),
                enabled,
                false,
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
                            .child(badges),
                    )
                    .child(toggle),
            );
        }

        div()
            .id("pi-extensions-scroll")
            .size_full()
            .overflow_y_scroll()
            .px(px(20.0))
            .pt(px(4.0))
            .child(
                div()
                    .w_full()
                    .max_w(px(CONTENT_MAX_WIDTH))
                    .mx_auto()
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
                            .rounded(px(11.0))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.composer)
                            .px(px(14.0))
                            .py(px(4.0))
                            .child(rows),
                    ),
            )
            .into_any_element()
    }
}

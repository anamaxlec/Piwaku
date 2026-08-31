//! PIWAKU: provider-neutral tool progress extraction.
//!
//! Long-running tools report structured progress through their native update
//! payloads. Adapters turn those payloads into [`ActivityProgress`] so UI
//! layers never parse provider JSON. The generic renderer stays the fallback:
//! a tool without an adapter simply never carries progress.

use serde_json::Value;

use crate::model::ActivityProgress;

pub(crate) trait ToolProgressAdapter: Send + Sync {
    /// Whether this adapter handles the tool. Matched on the wire tool name.
    fn matches(&self, tool_name: &str) -> bool;
    /// Pull progress out of a tool update. `args` are the call arguments from
    /// `tool_execution_start` (None on later updates); `partial` is the
    /// update's own payload. `None` means "nothing new worth showing".
    fn extract(&self, args: Option<&Value>, partial: Option<&Value>) -> Option<ActivityProgress>;
}

/// pi-web-access search progress.
///
/// Verified against pi-web-access 0.24.2 `onUpdate` payloads:
/// `{phase: "search" | "searching" | "curating" | "generating-summary" |
/// "waiting-for-approval" | "curator-fallback", progress, currentQuery}`.
/// `progress` is a real 0..1 ratio of completed queries during "search";
/// later phases carry their own fixed fractions. Anything else — absent
/// fields, out-of-range numbers — degrades to indeterminate, never a guess.
struct WebSearchProgress;

const SEARCH_PHASES: [&str; 2] = ["search", "searching"];
const CURATOR_PHASES: [&str; 4] = [
    "curating",
    "generating-summary",
    "waiting-for-approval",
    "curator-fallback",
];

impl ToolProgressAdapter for WebSearchProgress {
    fn matches(&self, tool_name: &str) -> bool {
        tool_name == "web_search"
    }

    fn extract(&self, _args: Option<&Value>, partial: Option<&Value>) -> Option<ActivityProgress> {
        let details = partial?.get("details")?;
        let phase = details.get("phase").and_then(Value::as_str)?;
        let current_query = details
            .get("currentQuery")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if SEARCH_PHASES.contains(&phase) {
            // A real completed/total ratio when one is reported; a single
            // in-flight query reports none and stays indeterminate.
            let fraction = details
                .get("progress")
                .and_then(Value::as_f64)
                .filter(|value| (0.0..=1.0).contains(value) && *value > 0.0)
                .map(|value| value as f32);
            return Some(ActivityProgress {
                fraction,
                phase: Some(phase.to_owned()),
                status_text: current_query,
            });
        }
        if CURATOR_PHASES.contains(&phase) {
            let fraction = details
                .get("progress")
                .and_then(Value::as_f64)
                .filter(|value| (0.0..=1.0).contains(value) && *value > 0.0)
                .map(|value| value as f32);
            return Some(ActivityProgress {
                fraction,
                phase: Some(phase.to_owned()),
                status_text: None,
            });
        }
        None
    }
}

/// pi-web-access fetch progress. 0.24.2 emits a single `{phase: "fetch",
/// progress: 0}` frame with no per-URL signal, so this is deliberately
/// indeterminate — no fabricated percentages.
struct WebFetchProgress;

impl ToolProgressAdapter for WebFetchProgress {
    fn matches(&self, tool_name: &str) -> bool {
        tool_name == "fetch_content"
    }

    fn extract(&self, _args: Option<&Value>, partial: Option<&Value>) -> Option<ActivityProgress> {
        let details = partial?.get("details")?;
        if details.get("phase").and_then(Value::as_str) != Some("fetch") {
            return None;
        }
        Some(ActivityProgress {
            fraction: None,
            phase: Some("fetch".to_owned()),
            status_text: None,
        })
    }
}

/// Registry consulted by the Pi driver on every tool update. Order is
/// irrelevant today (disjoint matchers) but kept deterministic regardless.
pub(super) static ADAPTERS: [&dyn ToolProgressAdapter; 2] = [&WebSearchProgress, &WebFetchProgress];

pub(super) fn extract_progress(
    tool_name: Option<&str>,
    args: Option<&Value>,
    partial: Option<&Value>,
) -> Option<ActivityProgress> {
    let tool_name = tool_name?;
    ADAPTERS
        .iter()
        .filter(|adapter| adapter.matches(tool_name))
        .find_map(|adapter| adapter.extract(args, partial))
}

/// PIWAKU: the settled one-line summary a completed web-access tool shows
/// under its row, mirroring the Pi TUI's status line ("11 sources", "Go Plan
/// | Command Code (8529 chars)"). Formulas verified against pi-web-access
/// 0.24.2's own renderer.
pub(super) fn completion_summary(
    tool_name: Option<&str>,
    result: Option<&Value>,
) -> Option<String> {
    let tool_name = tool_name?;
    let details = result?.get("details")?;
    match tool_name {
        "web_search" => {
            let total = details
                .get("totalResults")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let query_count = details.get("queryCount").and_then(Value::as_u64);
            let successful = details.get("successfulQueries").and_then(Value::as_u64);
            let summary = match (successful, query_count) {
                (Some(done), Some(total_queries)) if total_queries > 1 => tr!(
                    "activity.summary.queries",
                    done = done,
                    total = total_queries,
                    sources = total
                ),
                _ => tr!("activity.summary.sources", count = total),
            };
            let curated = details.get("curated").and_then(Value::as_bool) == Some(true);
            let curated_from = details.get("curatedFrom").and_then(Value::as_u64);
            match (curated, query_count, curated_from) {
                (true, Some(qc), Some(cf)) => Some(tr!(
                    "activity.summary.curated",
                    summary = summary,
                    kept = qc,
                    from = cf
                )),
                _ => Some(summary),
            }
        }
        "fetch_content" => {
            let url_count = details.get("urlCount").and_then(Value::as_u64);
            if url_count.is_some_and(|count| count > 1) {
                let total = details.get("totalChars").and_then(Value::as_u64);
                return Some(match total {
                    Some(chars) => tr!(
                        "activity.summary.urls_chars",
                        count = url_count.unwrap(),
                        chars = chars
                    ),
                    None => tr!("activity.summary.urls", count = url_count.unwrap()),
                });
            }
            let title = details
                .get("title")
                .and_then(Value::as_str)
                .filter(|title| !title.trim().is_empty())
                .unwrap_or("Untitled");
            let chars = details
                .get("totalChars")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let mut summary = tr!("activity.summary.chars", title = title, chars = chars);
            let images = details
                .get("imageCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if images > 0 {
                summary.push_str(&tr!("activity.summary.images", count = images));
            }
            if details.get("truncated").and_then(Value::as_bool) == Some(true) {
                summary.push_str(&tr!("activity.summary.truncated"));
            }
            Some(summary)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn web_search_reports_query_ratio_and_current_query() {
        let progress = extract_progress(
            Some("web_search"),
            None,
            Some(&json!({
                "content": [],
                "details": {"phase": "search", "progress": 0.5, "currentQuery": "GPT-5.6 sparse attention"}
            })),
        )
        .expect("search progress parses");
        assert_eq!(progress.fraction, Some(0.5));
        assert_eq!(progress.phase.as_deref(), Some("search"));
        assert_eq!(
            progress.status_text.as_deref(),
            Some("GPT-5.6 sparse attention")
        );
    }

    #[test]
    fn single_in_flight_query_is_indeterminate() {
        let progress = extract_progress(
            Some("web_search"),
            None,
            Some(&json!({"details": {"phase": "searching", "progress": 0, "currentQuery": "q"}})),
        )
        .expect("searching parses");
        assert_eq!(progress.fraction, None);
        assert_eq!(progress.status_text.as_deref(), Some("q"));
    }

    #[test]
    fn curator_phases_carry_through_without_status_text() {
        let progress = extract_progress(
            Some("web_search"),
            None,
            Some(&json!({"details": {"phase": "generating-summary", "progress": 0.9}})),
        )
        .expect("curator phase parses");
        assert_eq!(progress.fraction, Some(0.9));
        assert_eq!(progress.phase.as_deref(), Some("generating-summary"));
        assert_eq!(progress.status_text, None);
    }

    #[test]
    fn fetch_is_indeterminate_by_design() {
        let progress = extract_progress(
            Some("fetch_content"),
            None,
            Some(&json!({"details": {"phase": "fetch", "progress": 0}})),
        )
        .expect("fetch parses");
        assert_eq!(progress.fraction, None);
        assert_eq!(progress.phase.as_deref(), Some("fetch"));
    }

    #[test]
    fn completion_summary_mirrors_the_tui_status_lines() {
        let search = json!({
            "details": {
                "totalResults": 11, "queryCount": 3, "successfulQueries": 3,
                "curated": true, "curatedFrom": 5
            }
        });
        let expected = tr!(
            "activity.summary.curated",
            summary = tr!(
                "activity.summary.queries",
                done = 3,
                total = 3,
                sources = 11
            ),
            kept = 3,
            from = 5
        );
        assert_eq!(
            completion_summary(Some("web_search"), Some(&search)).as_deref(),
            Some(expected.as_str())
        );

        let single = json!({"details": {"totalResults": 11, "queryCount": 1}});
        assert_eq!(
            completion_summary(Some("web_search"), Some(&single)).as_deref(),
            Some(tr!("activity.summary.sources", count = 11).as_str())
        );

        let fetch = json!({"details": {"title": "Go Plan | Command Code", "totalChars": 8529}});
        assert_eq!(
            completion_summary(Some("fetch_content"), Some(&fetch)).as_deref(),
            Some(
                tr!(
                    "activity.summary.chars",
                    title = "Go Plan | Command Code",
                    chars = 8_529
                )
                .as_str()
            )
        );

        // Other tools and missing details carry no summary.
        assert_eq!(completion_summary(Some("bash"), Some(&search)), None);
        assert_eq!(completion_summary(Some("web_search"), None), None);
    }

    #[test]
    fn out_of_range_or_missing_fractions_degrade_to_indeterminate() {
        let progress = extract_progress(
            Some("web_search"),
            None,
            Some(&json!({"details": {"phase": "search", "progress": 7.5}})),
        )
        .expect("still parses");
        assert_eq!(progress.fraction, None);

        // Unrelated tools and malformed payloads never produce progress.
        assert_eq!(
            extract_progress(
                Some("bash"),
                None,
                Some(&json!({"details": {"phase": "search"}}))
            ),
            None
        );
        assert_eq!(
            extract_progress(Some("web_search"), None, Some(&json!({}))),
            None
        );
        assert_eq!(extract_progress(Some("web_search"), None, None), None);
    }
}

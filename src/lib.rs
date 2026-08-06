//! A [dprint](https://dprint.dev) Wasm plugin wrapping the
//! [fatou](https://fatou.dev) formatter for the Julia language.
//!
//! The plugin holds no formatting logic of its own. It maps dprint
//! configuration onto a [`fatou_formatter::FormatStyle`] and hands the file
//! text over; layout is entirely fatou's business.

use dprint_core::configuration::{
    ConfigKeyMap, ConfigurationDiagnostic, GlobalConfiguration, NewLineKind,
    get_unknown_property_diagnostics, get_value,
};
#[cfg(target_arch = "wasm32")]
use dprint_core::generate_plugin_code;
use dprint_core::plugins::{
    CheckConfigUpdatesMessage, ConfigChange, FileMatchingInfo, FormatError, FormatResult,
    PluginInfo, PluginResolveConfigurationResult, SyncFormatRequest, SyncHostFormatRequest,
    SyncPluginHandler,
};
use fatou_formatter::rowan::{TextRange, TextSize};
use fatou_formatter::{FormatStyle, LineEnding};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Extensions the plugin claims in dprint.
///
/// Deliberately the same set `fatou format` itself walks (see fatou's
/// `src/file_discovery.rs`, which collects only `.jl` files) rather than a
/// superset, so the plugin never formats something the CLI would skip.
const FILE_EXTENSIONS: &[&str] = &["jl"];

// The fallbacks used when neither the `fatou` config block nor the matching
// dprint global sets a value. They exist as functions rather than plain
// `#[serde(default)]` so the published schema advertises the real numbers
// instead of `u32`/`String`'s zero values. They mirror
// `FormatStyle::default()` (Julia conventions: width 92, indent 4).
fn default_line_width() -> u32 {
    92
}

fn default_indent_width() -> u32 {
    4
}

fn default_line_ending_value() -> String {
    "auto".to_string()
}

/// dprint-facing configuration, serialized as camelCase.
///
/// `lineEnding` is stored as a `String` and parsed lazily, borrowing its JSON
/// schema from [`LineEnding`] so the published `schema.json` tracks fatou's
/// accepted values instead of hand-listing them.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Configuration {
    /// Maximum line width the layout engine targets. Defaults to dprint's
    /// global `lineWidth`, or 92 if unset.
    #[serde(default = "default_line_width")]
    line_width: u32,
    /// Number of spaces per indentation level. Defaults to dprint's global
    /// `indentWidth`, or 4 if unset. Fatou always indents with spaces, so
    /// dprint's global `useTabs` has no effect.
    #[serde(default = "default_indent_width")]
    indent_width: u32,
    /// Line-ending style for formatted output. Defaults to dprint's global
    /// `newLineKind`, or `auto` if unset.
    #[serde(default = "default_line_ending_value")]
    #[schemars(with = "LineEnding")]
    line_ending: String,
}

#[derive(Default)]
pub struct FatouHandler;

impl FatouHandler {
    #[must_use]
    pub const fn new() -> Self {
        FatouHandler
    }
}

/// Parses a `lineEnding` config value, reporting a diagnostic on an unknown one.
///
/// The wire values are [`LineEnding`]'s own serde spellings (lowercase), so
/// they stay in step with the schema borrowed from it.
fn parse_line_ending(value: &str, diagnostics: &mut Vec<ConfigurationDiagnostic>) -> LineEnding {
    match value.to_ascii_lowercase().as_str() {
        "auto" => LineEnding::Auto,
        "lf" => LineEnding::Lf,
        "crlf" => LineEnding::Crlf,
        "native" => LineEnding::Native,
        other => {
            diagnostics.push(ConfigurationDiagnostic {
                property_name: "lineEnding".to_string(),
                message: format!(
                    "Unknown line ending '{other}'. Expected one of: auto, lf, crlf, native."
                ),
            });
            LineEnding::Auto
        }
    }
}

/// Maps dprint's global `newLineKind` onto a `lineEnding` default.
///
/// dprint has no equivalent of fatou's `native`, so an unset global falls back
/// to `auto`.
fn default_line_ending(global_config: &GlobalConfiguration) -> String {
    match global_config.new_line_kind {
        Some(NewLineKind::LineFeed) => "lf".to_string(),
        Some(NewLineKind::CarriageReturnLineFeed) => "crlf".to_string(),
        Some(NewLineKind::Auto) | None => default_line_ending_value(),
    }
}

fn build_style(cfg: &Configuration) -> FormatStyle {
    // Diagnostics were already reported at resolve time; discard them here.
    let mut throwaway = Vec::new();
    FormatStyle {
        line_width: cfg.line_width,
        indent_width: cfg.indent_width,
        line_ending: parse_line_ending(&cfg.line_ending, &mut throwaway),
    }
}

/// Renders a fatou format error as a dprint-facing message.
fn format_error(err: &fatou_formatter::FormatError) -> FormatError {
    FormatError::new(err.to_string())
}

/// Formats only `range`, splicing the result back into `text`.
///
/// `format_range` widens the requested range out to statement boundaries and
/// reports what it actually covered, so the splice has to use the returned
/// range rather than the requested one. Unlike its arity counterpart it has
/// already applied the target line ending to its text, so the replacement is
/// spliced as-is.
///
/// Parse diagnostics are deliberately not checked: fatou's formatter is total
/// over broken input (ERROR nodes lower transparently, byte-identical), and
/// its walking-skeleton grammar can report diagnostics on valid Julia, so
/// refusing here would reject real files the CLI formats happily.
fn format_text_range(
    text: &str,
    range: std::ops::Range<usize>,
    style: FormatStyle,
) -> Result<Option<String>, FormatError> {
    let start = TextSize::try_from(range.start)
        .map_err(|_| FormatError::new("format range start does not fit in the file"))?;
    let end = TextSize::try_from(range.end)
        .map_err(|_| FormatError::new("format range end does not fit in the file"))?;
    if start > end {
        return Err(FormatError::new("format range start is after its end"));
    }
    if usize::from(end) > text.len() {
        return Err(FormatError::new(
            "format range extends past the end of file",
        ));
    }

    let root = fatou_formatter::parser::parse(text).cst;

    let Some(formatted) = fatou_formatter::format_range(&root, TextRange::new(start, end), style)
        .map_err(|e| format_error(&e))?
    else {
        return Ok(None);
    };

    let replaced_start = usize::from(formatted.range.start());
    let replaced_end = usize::from(formatted.range.end());

    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..replaced_start]);
    out.push_str(&formatted.text);
    out.push_str(&text[replaced_end..]);
    Ok(Some(out))
}

impl SyncPluginHandler<Configuration> for FatouHandler {
    fn resolve_config(
        &mut self,
        config: ConfigKeyMap,
        global_config: &GlobalConfiguration,
    ) -> PluginResolveConfigurationResult<Configuration> {
        let mut config = config;
        let mut diagnostics = Vec::new();

        let line_width: u32 = get_value(
            &mut config,
            "lineWidth",
            global_config.line_width.unwrap_or_else(default_line_width),
            &mut diagnostics,
        );
        let indent_width: u32 = get_value(
            &mut config,
            "indentWidth",
            global_config
                .indent_width
                .map(u32::from)
                .unwrap_or_else(default_indent_width),
            &mut diagnostics,
        );
        let line_ending: String = get_value(
            &mut config,
            "lineEnding",
            default_line_ending(global_config),
            &mut diagnostics,
        );

        // Re-run the parse purely to surface a diagnostic for a bad value.
        let _ = parse_line_ending(&line_ending, &mut diagnostics);

        diagnostics.extend(get_unknown_property_diagnostics(config));

        PluginResolveConfigurationResult {
            config: Configuration {
                line_width,
                indent_width,
                line_ending,
            },
            diagnostics,
            file_matching: FileMatchingInfo {
                file_extensions: FILE_EXTENSIONS.iter().map(|s| (*s).to_string()).collect(),
                file_names: Vec::new(),
            },
        }
    }

    fn plugin_info(&mut self) -> PluginInfo {
        let version = env!("CARGO_PKG_VERSION").to_string();
        PluginInfo {
            name: env!("CARGO_PKG_NAME").to_string(),
            version: version.clone(),
            config_key: "fatou".to_string(),
            help_url: "https://fatou.dev".to_string(),
            config_schema_url: format!(
                "https://github.com/jolars/dprint-plugin-fatou/releases/download/v{version}/schema.json"
            ),
            update_url: Some("https://plugins.dprint.dev/jolars/fatou/latest.json".to_string()),
        }
    }

    fn license_text(&mut self) -> String {
        include_str!("../LICENSE").to_string()
    }

    fn check_config_updates(
        &self,
        _message: CheckConfigUpdatesMessage,
    ) -> Result<Vec<ConfigChange>, FormatError> {
        Ok(Vec::new())
    }

    fn format(
        &mut self,
        request: SyncFormatRequest<Configuration>,
        _format_with_host: impl FnMut(SyncHostFormatRequest) -> FormatResult,
    ) -> FormatResult {
        let text = String::from_utf8(request.file_bytes)
            .map_err(|e| FormatError::new(format!("input is not valid UTF-8: {e}")))?;

        let style = build_style(request.config);

        // fatou's API is `Result`-returning, so this is belt-and-braces: it
        // keeps an unexpected panic from tearing down the wasm instance.
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match request.range {
                None => fatou_formatter::format_with_style(&text, style)
                    .map(Some)
                    .map_err(|e| format_error(&e)),
                Some(range) => format_text_range(&text, range, style),
            }));

        let formatted = match result {
            Ok(formatted) => formatted?,
            Err(payload) => {
                let message = if let Some(s) = payload.downcast_ref::<&'static str>() {
                    (*s).to_string()
                } else if let Some(s) = payload.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "fatou panicked while formatting".to_string()
                };
                return Err(FormatError::new(format!("fatou panicked: {message}")));
            }
        };

        match formatted {
            Some(formatted) if formatted != text => Ok(Some(formatted.into_bytes())),
            _ => Ok(None),
        }
    }
}

#[cfg(target_arch = "wasm32")]
generate_plugin_code!(FatouHandler, FatouHandler::new());

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Configuration {
        Configuration {
            line_width: 92,
            indent_width: 4,
            line_ending: "auto".to_string(),
        }
    }

    fn format_all(cfg: &Configuration, text: &str) -> String {
        fatou_formatter::format_with_style(text, build_style(cfg)).expect("format should succeed")
    }

    const BLOCK_SOURCE: &str = "function f(x)\ny=x+1\ny*2\nend\n";

    #[test]
    fn formats_whole_file() {
        let cfg = config();
        assert_eq!(
            format_all(&cfg, BLOCK_SOURCE),
            "function f(x)\n    y = x + 1\n    y * 2\nend\n"
        );
    }

    #[test]
    fn honors_indent_width() {
        let mut cfg = config();
        cfg.indent_width = 8;
        assert_eq!(
            format_all(&cfg, BLOCK_SOURCE),
            "function f(x)\n        y = x + 1\n        y * 2\nend\n"
        );
    }

    #[test]
    fn honors_line_width() {
        let mut cfg = config();
        cfg.line_width = 20;
        let narrow = format_all(&cfg, "result = some_function(alpha, beta, gamma, delta)\n");
        assert!(
            narrow.lines().count() > 1,
            "a 20-column budget should force a break: {narrow:?}"
        );
    }

    #[test]
    fn crlf_input_round_trips_under_auto() {
        let cfg = config();
        let out = format_all(&cfg, "x=1\r\ny=2\r\n");
        assert_eq!(out, "x = 1\r\ny = 2\r\n");
    }

    #[test]
    fn explicit_line_ending_overrides_source() {
        let mut cfg = config();
        cfg.line_ending = "crlf".to_string();
        assert_eq!(format_all(&cfg, "x=1\ny=2\n"), "x = 1\r\ny = 2\r\n");
    }

    /// Fatou's formatter is total over broken input: ERROR nodes lower
    /// transparently (byte-identical) while recognized statements still
    /// format. The plugin must not refuse -- the walking-skeleton grammar can
    /// report diagnostics on valid Julia, and the CLI formats such files too.
    #[test]
    fn broken_input_formats_without_error() {
        let cfg = config();
        assert_eq!(
            format_all(&cfg, "function f(\nx=1\n"),
            "function f(\nx = 1\n\n"
        );
    }

    #[test]
    fn range_format_only_touches_its_range() {
        let cfg = config();
        let text = "a=1\nb  =2\n";
        let start = text
            .find("b  =2")
            .expect("fixture should contain the statement");
        let out = format_text_range(text, start..start + 5, build_style(&cfg))
            .expect("range format should succeed")
            .expect("range should cover a statement");
        assert_eq!(out, "a=1\nb = 2\n");
    }

    #[test]
    fn range_format_keeps_crlf_whole() {
        let cfg = config();
        let text = "a=1\r\nfunction f(x)\r\nx+1\r\nend\r\n";
        let start = text
            .find("function")
            .expect("fixture should contain the function");
        let out = format_text_range(text, start..text.len(), build_style(&cfg))
            .expect("range format should succeed")
            .expect("range should cover a statement");
        assert!(
            !out.replace("\r\n", "").contains('\n'),
            "spliced text left a bare LF behind: {out:?}"
        );
    }

    #[test]
    fn range_outside_the_file_is_an_error() {
        let cfg = config();
        assert!(format_text_range("a=1\n", 0..999, build_style(&cfg)).is_err());
    }

    #[test]
    fn default_line_ending_follows_the_dprint_global() {
        let global = |kind| GlobalConfiguration {
            line_width: None,
            use_tabs: None,
            indent_width: None,
            new_line_kind: kind,
        };
        assert_eq!(default_line_ending(&global(None)), "auto");
        assert_eq!(
            default_line_ending(&global(Some(NewLineKind::Auto))),
            "auto"
        );
        assert_eq!(
            default_line_ending(&global(Some(NewLineKind::LineFeed))),
            "lf"
        );
        assert_eq!(
            default_line_ending(&global(Some(NewLineKind::CarriageReturnLineFeed))),
            "crlf"
        );
    }

    #[test]
    fn unknown_line_ending_reports_a_diagnostic() {
        let mut diagnostics = Vec::new();
        assert_eq!(
            parse_line_ending("bogus", &mut diagnostics),
            LineEnding::Auto
        );
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].property_name, "lineEnding");
    }
}

#[cfg(test)]
mod schema_tests {
    use super::Configuration;

    const SCHEMA_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/schema.json");

    fn generated_schema() -> String {
        let schema = schemars::schema_for!(Configuration);
        let mut out = serde_json::to_string_pretty(&schema).expect("schema should serialize");
        out.push('\n');
        out
    }

    #[test]
    fn committed_schema_is_in_sync() {
        let generated = generated_schema();
        if std::env::var_os("UPDATE_SCHEMA").is_some() {
            std::fs::write(SCHEMA_PATH, &generated).expect("schema should be writable");
            return;
        }
        let committed = std::fs::read_to_string(SCHEMA_PATH)
            .expect("schema.json should exist; run `UPDATE_SCHEMA=1 cargo test` to create it");
        assert_eq!(
            committed, generated,
            "schema.json is stale; regenerate with `UPDATE_SCHEMA=1 cargo test`"
        );
    }

    /// The schema is what editors show users, so the advertised defaults have
    /// to be the real fallbacks rather than `u32`/`String`'s zero values.
    #[test]
    fn schema_advertises_the_real_defaults() {
        let schema: serde_json::Value =
            serde_json::from_str(&generated_schema()).expect("schema should parse");
        let props = &schema["properties"];
        assert_eq!(props["lineWidth"]["default"], serde_json::json!(92));
        assert_eq!(props["indentWidth"]["default"], serde_json::json!(4));
        assert_eq!(props["lineEnding"]["default"], serde_json::json!("auto"));
    }

    /// Guards against an upstream serde-rename change leaking PascalCase
    /// variants into the published schema.
    #[test]
    fn line_ending_values_stay_lowercase() {
        let schema = generated_schema();
        for expected in ["\"auto\"", "\"lf\"", "\"crlf\"", "\"native\""] {
            assert!(schema.contains(expected), "schema is missing {expected}");
        }
        for unexpected in ["\"Auto\"", "\"Lf\"", "\"Crlf\"", "\"Native\""] {
            assert!(
                !schema.contains(unexpected),
                "schema leaked a PascalCase variant: {unexpected}"
            );
        }
    }
}

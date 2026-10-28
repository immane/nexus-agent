//! Terminal presentation preferences, independent of any terminal backend.

use serde::{Deserialize, Serialize};

use crate::{ConfigError, store::invalid};

/// Presentation-only settings. Missing sections and fields retain defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TuiSettings {
    pub theme: ThemeSettings,
    pub display: DisplaySettings,
    pub input: InputSettings,
    pub render: RenderSettings,
}

/// Semantic colors. Values accept `default`, snake-case ANSI names,
/// `#RRGGBB`, or `index:0` through `index:255`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeSettings {
    pub foreground: String,
    pub background: String,
    pub accent: String,
    pub warning: String,
    pub danger: String,
    pub info: String,
    pub muted: String,
    pub secondary: String,
    pub modal_background: String,
    pub selected_background: String,
    pub selected_text: String,
    pub completion_text: String,
    pub completion_match: String,
    pub user_title: String,
    pub assistant_title: String,
    pub tool_title: String,
    pub system_title: String,
    pub markdown_code: String,
    pub markdown_heading: String,
    pub markdown_link: String,
    pub banner: [String; 6],
}

impl Default for ThemeSettings {
    fn default() -> Self {
        Self {
            foreground: "default".into(),
            background: "default".into(),
            accent: "green".into(),
            warning: "yellow".into(),
            danger: "red".into(),
            info: "cyan".into(),
            muted: "dark_gray".into(),
            secondary: "gray".into(),
            modal_background: "black".into(),
            selected_background: "dark_gray".into(),
            selected_text: "black".into(),
            completion_text: "white".into(),
            completion_match: "light_yellow".into(),
            user_title: "light_green".into(),
            assistant_title: "light_cyan".into(),
            tool_title: "yellow".into(),
            system_title: "dark_gray".into(),
            markdown_code: "yellow".into(),
            markdown_heading: "light_blue".into(),
            markdown_link: "cyan".into(),
            banner: [
                "#4ADE80", "#3FD074", "#34C269", "#2AB45E", "#20A854", "#16A34A",
            ]
            .map(String::from),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DisplaySettings {
    pub tool_preview_lines: usize,
    pub composer_max_rows: u16,
    pub picker_visible_rows: usize,
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self {
            tool_preview_lines: 10,
            composer_max_rows: 5,
            picker_visible_rows: 10,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClipboardMode {
    #[default]
    Auto,
    System,
    Osc52,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InputSettings {
    pub mouse_capture: bool,
    pub scroll_lines: usize,
    pub clipboard: ClipboardMode,
    pub copy_on_select: bool,
}

impl Default for InputSettings {
    fn default() -> Self {
        Self {
            mouse_capture: true,
            scroll_lines: 3,
            clipboard: ClipboardMode::Auto,
            copy_on_select: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RenderSettings {
    pub redraw_interval_ms: u64,
    /// Blink half-period; the frontend measures this in 50ms UI ticks.
    pub caret_blink_interval_ms: u64,
}

impl Default for RenderSettings {
    fn default() -> Self {
        Self {
            redraw_interval_ms: 33,
            caret_blink_interval_ms: 500,
        }
    }
}

impl TuiSettings {
    /// Checks bounded geometry and timing, plus every theme color. Preview
    /// lines allow 1..=1024, composer/picker rows 1..=100, wheel lines
    /// 1..=1000, redraw milliseconds 1..=1000, and blink milliseconds
    /// 50..=10000 in 50ms increments.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(1..=1024).contains(&self.display.tool_preview_lines)
            || !(1..=100).contains(&self.display.composer_max_rows)
            || !(1..=100).contains(&self.display.picker_visible_rows)
            || !(1..=1000).contains(&self.input.scroll_lines)
            || !(1..=1000).contains(&self.render.redraw_interval_ms)
            || !(50..=10_000).contains(&self.render.caret_blink_interval_ms)
            || !self.render.caret_blink_interval_ms.is_multiple_of(50)
        {
            return Err(invalid(
                "TUI preference is out of bounds (caret blink must use 50ms increments)",
            ));
        }
        let theme = &self.theme;
        let colors = [
            &theme.foreground,
            &theme.background,
            &theme.accent,
            &theme.warning,
            &theme.danger,
            &theme.info,
            &theme.muted,
            &theme.secondary,
            &theme.modal_background,
            &theme.selected_background,
            &theme.selected_text,
            &theme.completion_text,
            &theme.completion_match,
            &theme.user_title,
            &theme.assistant_title,
            &theme.tool_title,
            &theme.system_title,
            &theme.markdown_code,
            &theme.markdown_heading,
            &theme.markdown_link,
        ];
        if !colors
            .into_iter()
            .chain(theme.banner.iter())
            .all(|color| valid_color(color))
        {
            return Err(invalid("invalid TUI theme color"));
        }
        Ok(())
    }
}

fn valid_color(color: &str) -> bool {
    if matches!(
        color,
        "default"
            | "black"
            | "red"
            | "green"
            | "yellow"
            | "blue"
            | "magenta"
            | "cyan"
            | "gray"
            | "dark_gray"
            | "light_red"
            | "light_green"
            | "light_yellow"
            | "light_blue"
            | "light_magenta"
            | "light_cyan"
            | "white"
    ) {
        return true;
    }
    if let Some(hex) = color.strip_prefix('#') {
        return hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit());
    }
    color.strip_prefix("index:").is_some_and(|index| {
        !index.is_empty()
            && index.bytes().all(|byte| byte.is_ascii_digit())
            && index.parse::<u8>().is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_color_forms_validate() {
        assert!(TuiSettings::default().validate().is_ok());
        for color in ["default", "light_blue", "#aAbBcC", "index:0", "index:255"] {
            assert!(valid_color(color));
        }
        for color in [
            "reset",
            "LightBlue",
            "#fff",
            "index:256",
            "index:+1",
            "\u{1b}[31m",
        ] {
            assert!(!valid_color(color));
        }
    }

    #[test]
    fn strict_nested_deserialization_and_bounds() {
        let defaults: TuiSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(defaults, TuiSettings::default());
        assert!(serde_json::from_str::<TuiSettings>(r#"{"input":{"unknown":true}}"#).is_err());
        let mut settings = defaults;
        settings.display.picker_visible_rows = 0;
        assert!(settings.validate().is_err());
        settings.display.picker_visible_rows = 2;
        settings.render.caret_blink_interval_ms = 51;
        assert!(settings.validate().is_err());
    }
}

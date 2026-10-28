//! Backend translation for configuration's validated color strings.

use ratatui::style::Color;

pub(crate) fn color(value: &str) -> Color {
    match value {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" => Color::Gray,
        "dark_gray" => Color::DarkGray,
        "light_red" => Color::LightRed,
        "light_green" => Color::LightGreen,
        "light_yellow" => Color::LightYellow,
        "light_blue" => Color::LightBlue,
        "light_magenta" => Color::LightMagenta,
        "light_cyan" => Color::LightCyan,
        "white" => Color::White,
        _ => {
            if let Some(index) = value
                .strip_prefix("index:")
                .and_then(|index| index.parse().ok())
            {
                return Color::Indexed(index);
            }
            if let Some(rgb) = value
                .strip_prefix('#')
                .filter(|hex| hex.len() == 6)
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            {
                return Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8);
            }
            Color::Reset
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_validated_color_forms() {
        assert_eq!(color("default"), Color::Reset);
        assert_eq!(color("light_blue"), Color::LightBlue);
        assert_eq!(color("index:255"), Color::Indexed(255));
        assert_eq!(color("#aAbBcC"), Color::Rgb(170, 187, 204));
    }
}

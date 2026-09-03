use ratatui::style::Color;
pub const OX_YELLOW: Color = Color::Rgb(0xF5, 0xCF, 0x65);
pub const OX_OFF_WHITE: Color = Color::Rgb(0xE0, 0xE0, 0xE0);
pub const OX_RED: Color = Color::Rgb(255, 145, 173);
pub const OX_GREEN_LIGHT: Color = Color::Rgb(0x48, 0xD5, 0x97);
#[cfg(test)]
pub const OX_GREEN_DARK: Color = Color::Rgb(0x11, 0x27, 0x25);
pub const OX_GREEN_DARKEST: Color = Color::Rgb(0x0B, 0x14, 0x18);
#[cfg(test)]
pub const OX_GRAY: Color = Color::Rgb(0x9C, 0x9F, 0xA0);
#[cfg(test)]
pub const OX_GRAY_DARK: Color = Color::Rgb(0x62, 0x66, 0x68);
#[cfg(test)]
pub const OX_WHITE: Color = Color::Rgb(0xE7, 0xE7, 0xE8);
#[cfg(test)]
pub const OX_PINK: Color = Color::Rgb(0xE6, 0x68, 0x86);
#[cfg(test)]
pub const OX_YELLOW_DIM: Color = Color::Rgb(0xAE, 0x96, 0x4E);
#[cfg(test)]
pub const TUI_BLACK: Color = Color::Rgb(0x1E, 0x1E, 0x22);
pub const TUI_YELLOW: Color = Color::Rgb(0xF1, 0xD7, 0x8F);
pub const TUI_GREEN: Color = Color::Rgb(0x8F, 0xEF, 0xBF);
#[cfg(test)]
pub const TUI_GREEN_DARK: Color = Color::Rgb(0x2E, 0x81, 0x60);
pub const TUI_GREY: Color = Color::Rgb(0x78, 0x78, 0x7A);
pub const TUI_PURPLE: Color = Color::Rgb(0xBE, 0x95, 0xEB);
#[cfg(test)]
pub const TUI_PURPLE_DIM: Color = Color::Rgb(0x6C, 0x55, 0x84);
pub const TUI_GREY_DARK: Color = Color::Rgb(66, 66, 69);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn palette_values_are_exact() {
        assert_eq!(
            [
                OX_YELLOW,
                OX_OFF_WHITE,
                OX_RED,
                OX_GREEN_LIGHT,
                OX_GREEN_DARK,
                OX_GREEN_DARKEST,
                OX_GRAY,
                OX_GRAY_DARK,
                OX_WHITE,
                OX_PINK,
                OX_YELLOW_DIM,
                TUI_BLACK,
                TUI_YELLOW,
                TUI_GREEN,
                TUI_GREEN_DARK,
                TUI_GREY,
                TUI_PURPLE,
                TUI_PURPLE_DIM,
                TUI_GREY_DARK
            ],
            [
                Color::Rgb(0xF5, 0xCF, 0x65),
                Color::Rgb(0xE0, 0xE0, 0xE0),
                Color::Rgb(255, 145, 173),
                Color::Rgb(0x48, 0xD5, 0x97),
                Color::Rgb(0x11, 0x27, 0x25),
                Color::Rgb(0x0B, 0x14, 0x18),
                Color::Rgb(0x9C, 0x9F, 0xA0),
                Color::Rgb(0x62, 0x66, 0x68),
                Color::Rgb(0xE7, 0xE7, 0xE8),
                Color::Rgb(0xE6, 0x68, 0x86),
                Color::Rgb(0xAE, 0x96, 0x4E),
                Color::Rgb(0x1E, 0x1E, 0x22),
                Color::Rgb(0xF1, 0xD7, 0x8F),
                Color::Rgb(0x8F, 0xEF, 0xBF),
                Color::Rgb(0x2E, 0x81, 0x60),
                Color::Rgb(0x78, 0x78, 0x7A),
                Color::Rgb(0xBE, 0x95, 0xEB),
                Color::Rgb(0x6C, 0x55, 0x84),
                Color::Rgb(66, 66, 69)
            ]
        );
    }
}

use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorScheme {
    System,
    Light,
    Dark,
}

impl ColorScheme {
    fn config_value(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    fn from_config_value(value: &str) -> Option<Self> {
        match value {
            "system" => Some(Self::System),
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }

    pub fn apply(self) {
        let scheme = match self {
            Self::System => adw::ColorScheme::Default,
            Self::Light => adw::ColorScheme::ForceLight,
            Self::Dark => adw::ColorScheme::ForceDark,
        };
        adw::StyleManager::default().set_color_scheme(scheme);
    }

    pub fn save(self) -> std::io::Result<()> {
        let path = settings_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, format!("color-scheme={}\n", self.config_value()))
    }
}

fn settings_path() -> PathBuf {
    glib::user_config_dir()
        .join("glimpse")
        .join("settings.ini")
}

fn parse_settings(contents: &str) -> ColorScheme {
    contents
        .lines()
        .find_map(|line| line.strip_prefix("color-scheme="))
        .and_then(ColorScheme::from_config_value)
        .unwrap_or(ColorScheme::System)
}

pub fn load() -> ColorScheme {
    std::fs::read_to_string(settings_path())
        .map(|contents| parse_settings(&contents))
        .unwrap_or(ColorScheme::System)
}

#[cfg(test)]
mod tests {
    use super::{parse_settings, ColorScheme};

    #[test]
    fn defaults_to_system_when_setting_is_missing_or_invalid() {
        assert_eq!(parse_settings(""), ColorScheme::System);
        assert_eq!(parse_settings("color-scheme=sepia\n"), ColorScheme::System);
    }

    #[test]
    fn parses_all_supported_color_schemes() {
        assert_eq!(parse_settings("color-scheme=system\n"), ColorScheme::System);
        assert_eq!(parse_settings("color-scheme=light\n"), ColorScheme::Light);
        assert_eq!(parse_settings("color-scheme=dark\n"), ColorScheme::Dark);
    }
}

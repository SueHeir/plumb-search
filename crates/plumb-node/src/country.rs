//! The searcher's home country: which country's sites get a small boost.

use plumb_core::normalize_country;

/// How a server picks the home country of a search that does not name one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum HomeCountry {
    /// From the browser's preferred language (`en-US` -> `US`), else from
    /// this computer's language and region settings.
    #[default]
    Auto,
    /// Always this ISO 3166-1 alpha-2 code.
    Fixed(String),
    /// No home country: every country's sites rank alike.
    Off,
}

impl HomeCountry {
    /// Reads a setting: `auto`, `any` (or `none`, `off`), or a two-letter
    /// country code (`us`, `GB`, `UK`).
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        match text.to_ascii_lowercase().as_str() {
            "auto" | "" => Ok(HomeCountry::Auto),
            "any" | "none" | "off" => Ok(HomeCountry::Off),
            _ => normalize_country(text).map(HomeCountry::Fixed).ok_or_else(|| {
                format!("expected a two-letter country code such as US or DE, `auto` or `any`, got {text:?}")
            }),
        }
    }

    /// The home country for a request with this `Accept-Language` header.
    pub fn resolve(&self, accept_language: Option<&str>) -> Option<String> {
        match self {
            HomeCountry::Fixed(code) => Some(code.clone()),
            HomeCountry::Off => None,
            HomeCountry::Auto => accept_language
                .and_then(country_from_accept_language)
                .or_else(system_country),
        }
    }
}

/// The country of the first language tag that names one: `en-US,en;q=0.9`
/// -> `US`, `de-DE` -> `DE`, `zh-Hant-TW` -> `TW`. `en` alone names none.
pub fn country_from_accept_language(header: &str) -> Option<String> {
    header
        .split(',')
        .map(|entry| entry.split(';').next().unwrap_or_default().trim())
        .find_map(country_from_locale)
}

/// The country of a locale such as `en-US`, `en_US.UTF-8` or `zh-Hant-TW`.
pub fn country_from_locale(locale: &str) -> Option<String> {
    let locale = locale.split(['.', '@']).next()?;
    locale
        .split(['-', '_'])
        .skip(1)
        .find(|part| part.len() == 2 && part.bytes().all(|b| b.is_ascii_alphabetic()))
        .and_then(normalize_country)
}

/// The country of this computer's language and region settings, if they name one.
pub fn system_country() -> Option<String> {
    sys_locale::get_locale()
        .as_deref()
        .and_then(country_from_locale)
}

/// Countries offered on the search page, by English name. Any other code
/// still works when given in the address.
pub use plumb_core::COUNTRY_CHOICES;

/// The English name of a country code, or the code itself.
pub fn country_name(code: &str) -> &str {
    COUNTRY_CHOICES
        .iter()
        .find(|(known, _)| *known == code)
        .map_or(code, |(_, name)| name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parse() {
        assert_eq!(HomeCountry::parse("auto"), Ok(HomeCountry::Auto));
        assert_eq!(HomeCountry::parse("ANY"), Ok(HomeCountry::Off));
        assert_eq!(
            HomeCountry::parse("de"),
            Ok(HomeCountry::Fixed("DE".into()))
        );
        assert_eq!(
            HomeCountry::parse("uk"),
            Ok(HomeCountry::Fixed("GB".into()))
        );
        assert!(HomeCountry::parse("Germany").is_err());
    }

    #[test]
    fn browsers_and_locales_name_countries() {
        assert_eq!(
            country_from_accept_language("en-US,en;q=0.9").as_deref(),
            Some("US")
        );
        assert_eq!(
            country_from_accept_language("en, de-DE;q=0.8").as_deref(),
            Some("DE")
        );
        assert_eq!(
            country_from_accept_language("zh-Hant-TW").as_deref(),
            Some("TW")
        );
        assert_eq!(country_from_accept_language("en, fr"), None);
        assert_eq!(country_from_accept_language("*"), None);
        assert_eq!(country_from_locale("en_GB.UTF-8").as_deref(), Some("GB"));
        assert_eq!(country_from_locale("C"), None);
        assert_eq!(country_from_locale("POSIX"), None);
    }

    #[test]
    fn resolving_prefers_the_setting() {
        let fixed = HomeCountry::Fixed("FR".into());
        assert_eq!(fixed.resolve(Some("en-US")).as_deref(), Some("FR"));
        assert_eq!(HomeCountry::Off.resolve(Some("en-US")), None);
        assert_eq!(
            HomeCountry::Auto.resolve(Some("de-AT")).as_deref(),
            Some("AT")
        );
    }

    #[test]
    fn choices_are_valid_and_sorted_by_name() {
        for (code, _) in COUNTRY_CHOICES {
            assert_eq!(normalize_country(code).as_deref(), Some(*code));
        }
        assert!(COUNTRY_CHOICES.windows(2).all(|w| w[0].1 < w[1].1));
        assert_eq!(country_name("DE"), "Germany");
        assert_eq!(country_name("LU"), "LU");
    }
}

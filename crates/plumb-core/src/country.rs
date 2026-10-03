//! Which country a site belongs to, for the "your country" setting.

use crate::SiteRecord;

/// Country-code top-level domains that are sold and used worldwide, like a
/// `.com`, so their sites belong to no country in particular: `.io`, `.co`,
/// `.tv`, `.me`, `.ai`, `.fm` and so on. `.eu` is the European Union's.
const GENERIC_CCTLDS: &[&str] = &[
    "ac", "ad", "ai", "as", "bz", "cc", "cd", "co", "cx", "dj", "eu", "fm", "gg", "gl", "gs", "im",
    "io", "je", "la", "ly", "md", "me", "ms", "mu", "nu", "pw", "sh", "so", "st", "su", "tk", "tm",
    "to", "tv", "vc", "ws",
];

/// The country a domain ending stands for, as an ISO 3166-1 alpha-2 code:
/// `example.fr` -> `FR`, `bbc.co.uk` -> `GB`, `gc.ca` -> `CA`. `.gov`,
/// `.mil` and `.edu`, which only United States institutions can register,
/// are `US`. Generic endings (`.com`, `.org`) and country codes used
/// worldwide ([`GENERIC_CCTLDS`]: `.io`, `.tv`, `.co`) have no country.
pub fn tld_country(domain: &str) -> Option<&'static str> {
    let tld = domain.trim().trim_end_matches('.').rsplit('.').next()?;
    let tld = tld.to_ascii_lowercase();
    match tld.as_str() {
        "gov" | "mil" | "edu" => return Some("US"),
        "uk" => return Some("GB"),
        _ => {}
    }
    if tld.len() != 2 || !tld.bytes().all(|b| b.is_ascii_lowercase()) {
        return None;
    }
    if GENERIC_CCTLDS.contains(&tld.as_str()) {
        return None;
    }
    COUNTRY_CODES
        .iter()
        .find(|code| code.eq_ignore_ascii_case(&tld))
        .copied()
}

/// The country a site belongs to: its [`SiteRecord::country`] from
/// Wikidata when it has one, otherwise its domain ending's
/// ([`tld_country`]). `None` for global sites such as most `.com`s.
pub fn site_country(record: &SiteRecord) -> Option<String> {
    record
        .country
        .as_deref()
        .and_then(normalize_country)
        .or_else(|| tld_country(&record.domain).map(str::to_string))
}

/// `us`, ` US ` -> `US`; `UK` -> `GB`. `None` unless it is a two-letter
/// ISO 3166-1 alpha-2 code.
pub fn normalize_country(code: &str) -> Option<String> {
    let code = code.trim().to_ascii_uppercase();
    let code = if code == "UK" { "GB".to_string() } else { code };
    COUNTRY_CODES.contains(&code.as_str()).then_some(code)
}

/// ISO 3166-1 alpha-2 codes.
const COUNTRY_CODES: &[&str] = &[
    "AD", "AE", "AF", "AG", "AI", "AL", "AM", "AO", "AQ", "AR", "AS", "AT", "AU", "AW", "AX", "AZ",
    "BA", "BB", "BD", "BE", "BF", "BG", "BH", "BI", "BJ", "BL", "BM", "BN", "BO", "BQ", "BR", "BS",
    "BT", "BV", "BW", "BY", "BZ", "CA", "CC", "CD", "CF", "CG", "CH", "CI", "CK", "CL", "CM", "CN",
    "CO", "CR", "CU", "CV", "CW", "CX", "CY", "CZ", "DE", "DJ", "DK", "DM", "DO", "DZ", "EC", "EE",
    "EG", "EH", "ER", "ES", "ET", "FI", "FJ", "FK", "FM", "FO", "FR", "GA", "GB", "GD", "GE", "GF",
    "GG", "GH", "GI", "GL", "GM", "GN", "GP", "GQ", "GR", "GS", "GT", "GU", "GW", "GY", "HK", "HM",
    "HN", "HR", "HT", "HU", "ID", "IE", "IL", "IM", "IN", "IO", "IQ", "IR", "IS", "IT", "JE", "JM",
    "JO", "JP", "KE", "KG", "KH", "KI", "KM", "KN", "KP", "KR", "KW", "KY", "KZ", "LA", "LB", "LC",
    "LI", "LK", "LR", "LS", "LT", "LU", "LV", "LY", "MA", "MC", "MD", "ME", "MF", "MG", "MH", "MK",
    "ML", "MM", "MN", "MO", "MP", "MQ", "MR", "MS", "MT", "MU", "MV", "MW", "MX", "MY", "MZ", "NA",
    "NC", "NE", "NF", "NG", "NI", "NL", "NO", "NP", "NR", "NU", "NZ", "OM", "PA", "PE", "PF", "PG",
    "PH", "PK", "PL", "PM", "PN", "PR", "PS", "PT", "PW", "PY", "QA", "RE", "RO", "RS", "RU", "RW",
    "SA", "SB", "SC", "SD", "SE", "SG", "SH", "SI", "SJ", "SK", "SL", "SM", "SN", "SO", "SR", "SS",
    "ST", "SV", "SX", "SY", "SZ", "TC", "TD", "TF", "TG", "TH", "TJ", "TK", "TL", "TM", "TN", "TO",
    "TR", "TT", "TV", "TW", "TZ", "UA", "UG", "UM", "US", "UY", "UZ", "VA", "VC", "VE", "VG", "VI",
    "VN", "VU", "WF", "WS", "YE", "YT", "ZA", "ZM", "ZW",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_endings_name_countries() {
        assert_eq!(tld_country("americanairlines.fr"), Some("FR"));
        assert_eq!(tld_country("bbc.co.uk"), Some("GB"));
        assert_eq!(tld_country("veterans.gc.ca"), Some("CA"));
        assert_eq!(tld_country("ssa.gov"), Some("US"));
        assert_eq!(tld_country("mit.edu"), Some("US"));
        assert_eq!(tld_country("example.DE."), Some("DE"));
    }

    #[test]
    fn global_endings_have_no_country() {
        for domain in [
            "usbank.com",
            "wikipedia.org",
            "github.io",
            "twitch.tv",
            "x.ai",
            "europa.eu",
            "t.co",
            "xn--p1ai",
        ] {
            assert_eq!(tld_country(domain), None, "{domain}");
        }
        // Not a country code at all.
        assert_eq!(tld_country("example.zz"), None);
    }

    #[test]
    fn wikidata_country_beats_the_domain_ending() {
        let mut record = SiteRecord::new("nestle.com");
        assert_eq!(site_country(&record), None);
        record.country = Some("ch".into());
        assert_eq!(site_country(&record).as_deref(), Some("CH"));
        let mut record = SiteRecord::new("americanairlines.fr");
        assert_eq!(site_country(&record).as_deref(), Some("FR"));
        record.country = Some("bogus".into());
        assert_eq!(site_country(&record).as_deref(), Some("FR"));
    }

    #[test]
    fn country_codes_are_normalized() {
        assert_eq!(normalize_country(" us ").as_deref(), Some("US"));
        assert_eq!(normalize_country("UK").as_deref(), Some("GB"));
        assert_eq!(normalize_country("USA"), None);
        assert_eq!(normalize_country(""), None);
    }
}

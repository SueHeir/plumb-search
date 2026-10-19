//! Which country a site belongs to, for the "your country" setting
//! ([`crate::site::site_country`]).

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

/// The country of the site `domain`, which Wikidata says is of `country`:
/// that country when it is one, otherwise the domain ending's
/// ([`tld_country`]) ([`crate::site::site_country`]).
pub(crate) fn country_of(country: Option<&str>, domain: &str) -> Option<String> {
    country
        .and_then(normalize_country)
        .or_else(|| tld_country(domain).map(str::to_string))
}

/// `us`, ` US ` -> `US`; `UK` -> `GB`. `None` unless it is a two-letter
/// ISO 3166-1 alpha-2 code.
pub fn normalize_country(code: &str) -> Option<String> {
    let code = code.trim().to_ascii_uppercase();
    let code = if code == "UK" { "GB".to_string() } else { code };
    COUNTRY_CODES.contains(&code.as_str()).then_some(code)
}

/// Countries offered on the search page, by English name. Any other code
/// still works when given in the address.
pub const COUNTRY_CHOICES: &[(&str, &str)] = &[
    ("AR", "Argentina"),
    ("AU", "Australia"),
    ("AT", "Austria"),
    ("BE", "Belgium"),
    ("BR", "Brazil"),
    ("CA", "Canada"),
    ("CL", "Chile"),
    ("CN", "China"),
    ("CO", "Colombia"),
    ("CZ", "Czechia"),
    ("DK", "Denmark"),
    ("EG", "Egypt"),
    ("FI", "Finland"),
    ("FR", "France"),
    ("DE", "Germany"),
    ("GR", "Greece"),
    ("HK", "Hong Kong"),
    ("HU", "Hungary"),
    ("IN", "India"),
    ("ID", "Indonesia"),
    ("IE", "Ireland"),
    ("IL", "Israel"),
    ("IT", "Italy"),
    ("JP", "Japan"),
    ("KE", "Kenya"),
    ("MY", "Malaysia"),
    ("MX", "Mexico"),
    ("NL", "Netherlands"),
    ("NZ", "New Zealand"),
    ("NG", "Nigeria"),
    ("NO", "Norway"),
    ("PK", "Pakistan"),
    ("PE", "Peru"),
    ("PH", "Philippines"),
    ("PL", "Poland"),
    ("PT", "Portugal"),
    ("RO", "Romania"),
    ("SA", "Saudi Arabia"),
    ("SG", "Singapore"),
    ("ZA", "South Africa"),
    ("KR", "South Korea"),
    ("ES", "Spain"),
    ("SE", "Sweden"),
    ("CH", "Switzerland"),
    ("TW", "Taiwan"),
    ("TH", "Thailand"),
    ("TR", "Turkey"),
    ("UA", "Ukraine"),
    ("AE", "United Arab Emirates"),
    ("GB", "United Kingdom"),
    ("US", "United States"),
    ("VN", "Vietnam"),
];

/// The code of the country named `name` in English ("germany" -> `DE`),
/// of those in [`COUNTRY_CHOICES`] plus a few common Spanish/German names.
pub fn country_of_name(name: &str) -> Option<&'static str> {
    let name = crate::normalize_text(name);
    let other = match name.as_str() {
        "usa" | "us" | "america" | "united states of america" => Some("US"),
        "uk" | "england" | "scotland" | "wales" | "great britain" | "britain" => Some("GB"),
        "méxico" => Some("MX"),
        "españa" | "espana" | "spanien" => Some("ES"),
        "deutschland" | "alemania" => Some("DE"),
        "francia" | "frankreich" => Some("FR"),
        "italia" | "italien" => Some("IT"),
        "estados unidos" | "vereinigte staaten" => Some("US"),
        "reino unido" | "vereinigtes königreich" => Some("GB"),
        _ => None,
    };
    other.or_else(|| {
        COUNTRY_CHOICES
            .iter()
            .find(|(_, english)| crate::normalize_text(english) == name)
            .map(|(code, _)| *code)
    })
}

/// The code of any country named `name` in English, past
/// [`country_of_name`]: "iceland", "croatia", "czech republic". Only for
/// saying where a place is ("hotels in reykjavik iceland"): elsewhere a
/// brand's name can be a country's ("air jordan").
pub fn country_of_any_name(name: &str) -> Option<&'static str> {
    country_of_name(name).or_else(|| {
        let name = crate::normalize_text(name);
        let other = match name.as_str() {
            "czech republic" => Some("CZ"),
            "holland" | "the netherlands" => Some("NL"),
            "uae" => Some("AE"),
            "korea" => Some("KR"),
            "burma" => Some("MM"),
            "cote divoire" | "côte divoire" => Some("CI"),
            "swaziland" => Some("SZ"),
            "macedonia" => Some("MK"),
            "bosnia" => Some("BA"),
            "drc" | "dr congo" => Some("CD"),
            "curaçao" => Some("CW"),
            "the bahamas" => Some("BS"),
            "the gambia" => Some("GM"),
            "turkiye" | "türkiye" => Some("TR"),
            "timor leste" => Some("TL"),
            "cabo verde" => Some("CV"),
            "russian federation" => Some("RU"),
            _ => None,
        };
        other.or_else(|| {
            OTHER_COUNTRIES
                .iter()
                .find(|(_, english)| crate::normalize_text(english) == name)
                .map(|(code, _)| *code)
        })
    })
}

/// The countries not in [`COUNTRY_CHOICES`], by English name.
const OTHER_COUNTRIES: &[(&str, &str)] = &[
    ("AD", "Andorra"),
    ("AF", "Afghanistan"),
    ("AG", "Antigua and Barbuda"),
    ("AL", "Albania"),
    ("AM", "Armenia"),
    ("AO", "Angola"),
    ("AW", "Aruba"),
    ("AZ", "Azerbaijan"),
    ("BA", "Bosnia and Herzegovina"),
    ("BB", "Barbados"),
    ("BD", "Bangladesh"),
    ("BF", "Burkina Faso"),
    ("BG", "Bulgaria"),
    ("BH", "Bahrain"),
    ("BI", "Burundi"),
    ("BJ", "Benin"),
    ("BM", "Bermuda"),
    ("BN", "Brunei"),
    ("BO", "Bolivia"),
    ("BS", "Bahamas"),
    ("BT", "Bhutan"),
    ("BW", "Botswana"),
    ("BY", "Belarus"),
    ("BZ", "Belize"),
    ("CD", "Democratic Republic of the Congo"),
    ("CF", "Central African Republic"),
    ("CG", "Congo"),
    ("CI", "Ivory Coast"),
    ("CM", "Cameroon"),
    ("CR", "Costa Rica"),
    ("CU", "Cuba"),
    ("CV", "Cape Verde"),
    ("CW", "Curacao"),
    ("CY", "Cyprus"),
    ("DJ", "Djibouti"),
    ("DM", "Dominica"),
    ("DO", "Dominican Republic"),
    ("DZ", "Algeria"),
    ("EC", "Ecuador"),
    ("EE", "Estonia"),
    ("ER", "Eritrea"),
    ("ET", "Ethiopia"),
    ("FJ", "Fiji"),
    ("FO", "Faroe Islands"),
    ("GA", "Gabon"),
    ("GD", "Grenada"),
    ("GE", "Georgia"),
    ("GH", "Ghana"),
    ("GI", "Gibraltar"),
    ("GL", "Greenland"),
    ("GM", "Gambia"),
    ("GN", "Guinea"),
    ("GQ", "Equatorial Guinea"),
    ("GT", "Guatemala"),
    ("GU", "Guam"),
    ("GW", "Guinea-Bissau"),
    ("GY", "Guyana"),
    ("HN", "Honduras"),
    ("HR", "Croatia"),
    ("HT", "Haiti"),
    ("IQ", "Iraq"),
    ("IR", "Iran"),
    ("IS", "Iceland"),
    ("JM", "Jamaica"),
    ("JO", "Jordan"),
    ("KG", "Kyrgyzstan"),
    ("KH", "Cambodia"),
    ("KI", "Kiribati"),
    ("KM", "Comoros"),
    ("KN", "Saint Kitts and Nevis"),
    ("KP", "North Korea"),
    ("KW", "Kuwait"),
    ("KY", "Cayman Islands"),
    ("KZ", "Kazakhstan"),
    ("LA", "Laos"),
    ("LB", "Lebanon"),
    ("LC", "Saint Lucia"),
    ("LI", "Liechtenstein"),
    ("LK", "Sri Lanka"),
    ("LR", "Liberia"),
    ("LS", "Lesotho"),
    ("LT", "Lithuania"),
    ("LU", "Luxembourg"),
    ("LV", "Latvia"),
    ("LY", "Libya"),
    ("MA", "Morocco"),
    ("MC", "Monaco"),
    ("MD", "Moldova"),
    ("ME", "Montenegro"),
    ("MG", "Madagascar"),
    ("MH", "Marshall Islands"),
    ("MK", "North Macedonia"),
    ("ML", "Mali"),
    ("MM", "Myanmar"),
    ("MN", "Mongolia"),
    ("MO", "Macau"),
    ("MR", "Mauritania"),
    ("MT", "Malta"),
    ("MU", "Mauritius"),
    ("MV", "Maldives"),
    ("MW", "Malawi"),
    ("MZ", "Mozambique"),
    ("NA", "Namibia"),
    ("NC", "New Caledonia"),
    ("NE", "Niger"),
    ("NI", "Nicaragua"),
    ("NP", "Nepal"),
    ("NR", "Nauru"),
    ("OM", "Oman"),
    ("PA", "Panama"),
    ("PF", "French Polynesia"),
    ("PG", "Papua New Guinea"),
    ("PR", "Puerto Rico"),
    ("PS", "Palestine"),
    ("PW", "Palau"),
    ("PY", "Paraguay"),
    ("QA", "Qatar"),
    ("RS", "Serbia"),
    ("RU", "Russia"),
    ("RW", "Rwanda"),
    ("SB", "Solomon Islands"),
    ("SC", "Seychelles"),
    ("SD", "Sudan"),
    ("SI", "Slovenia"),
    ("SK", "Slovakia"),
    ("SL", "Sierra Leone"),
    ("SM", "San Marino"),
    ("SN", "Senegal"),
    ("SO", "Somalia"),
    ("SR", "Suriname"),
    ("SS", "South Sudan"),
    ("ST", "Sao Tome and Principe"),
    ("SV", "El Salvador"),
    ("SY", "Syria"),
    ("SZ", "Eswatini"),
    ("TD", "Chad"),
    ("TG", "Togo"),
    ("TJ", "Tajikistan"),
    ("TL", "East Timor"),
    ("TM", "Turkmenistan"),
    ("TN", "Tunisia"),
    ("TO", "Tonga"),
    ("TT", "Trinidad and Tobago"),
    ("TV", "Tuvalu"),
    ("TZ", "Tanzania"),
    ("UG", "Uganda"),
    ("UY", "Uruguay"),
    ("UZ", "Uzbekistan"),
    ("VA", "Vatican City"),
    ("VC", "Saint Vincent and the Grenadines"),
    ("VE", "Venezuela"),
    ("VU", "Vanuatu"),
    ("WS", "Samoa"),
    ("YE", "Yemen"),
    ("ZM", "Zambia"),
    ("ZW", "Zimbabwe"),
];

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
    fn every_country_is_named_for_places() {
        for (name, code) in [
            ("Iceland", "IS"),
            ("croatia", "HR"),
            ("Czech Republic", "CZ"),
            ("Côte d'Ivoire", "CI"),
            ("Trinidad and Tobago", "TT"),
            ("germany", "DE"),
        ] {
            assert_eq!(country_of_any_name(name), Some(code), "{name}");
        }
        // Not where a brand's name may be one.
        assert_eq!(country_of_name("jordan"), None);
        assert_eq!(country_of_any_name("atlantis"), None);
        for (code, _) in OTHER_COUNTRIES {
            assert!(COUNTRY_CODES.contains(code), "{code}");
            assert!(COUNTRY_CHOICES.iter().all(|(chosen, _)| chosen != code));
        }
    }

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
        assert_eq!(country_of(None, "nestle.com"), None);
        assert_eq!(country_of(Some("ch"), "nestle.com").as_deref(), Some("CH"));
        let airline = "americanairlines.fr";
        assert_eq!(country_of(None, airline).as_deref(), Some("FR"));
        assert_eq!(country_of(Some("bogus"), airline).as_deref(), Some("FR"));
    }

    #[test]
    fn country_codes_are_normalized() {
        assert_eq!(normalize_country(" us ").as_deref(), Some("US"));
        assert_eq!(normalize_country("UK").as_deref(), Some("GB"));
        assert_eq!(normalize_country("USA"), None);
        assert_eq!(normalize_country(""), None);
    }
}

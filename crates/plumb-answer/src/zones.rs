//! The clock a town keeps, worked out from where it is, for towns the
//! list in `places.rs` lacks: its country's clock, or in the US, Canada
//! and Australia its state's, when all of it keeps one time. Where a
//! country keeps several, the town takes the clock of the zone whose city
//! is nearest only when every city keeping another time is much farther,
//! so a town near a line between two clocks gets no answer rather than a
//! wrong one.

use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};

/// `country|zone` for a country that keeps one time, and
/// `country|zone|lat|lon` for each zone (by its main city) of one that
/// keeps several. From the time zone database's `zone.tab` (2026c),
/// without the US, Canada and Australia (see [`STATES`]), Antarctica, and
/// Xinjiang's time, since China keeps one official time.
const COUNTRIES: &str = "\
AD|Europe/Andorra
AE|Asia/Dubai
AF|Asia/Kabul
AG|America/Antigua
AI|America/Anguilla
AL|Europe/Tirane
AM|Asia/Yerevan
AO|Africa/Luanda
AR|America/Argentina/Buenos_Aires
AS|Pacific/Pago_Pago
AT|Europe/Vienna
AW|America/Aruba
AX|Europe/Mariehamn
AZ|Asia/Baku
BA|Europe/Sarajevo
BB|America/Barbados
BD|Asia/Dhaka
BE|Europe/Brussels
BF|Africa/Ouagadougou
BG|Europe/Sofia
BH|Asia/Bahrain
BI|Africa/Bujumbura
BJ|Africa/Porto-Novo
BL|America/St_Barthelemy
BM|Atlantic/Bermuda
BN|Asia/Brunei
BO|America/La_Paz
BQ|America/Kralendijk
BR|America/Noronha|-3.85|-32.42
BR|America/Belem|-1.45|-48.48
BR|America/Fortaleza|-3.72|-38.5
BR|America/Recife|-8.05|-34.9
BR|America/Araguaina|-7.2|-48.2
BR|America/Maceio|-9.67|-35.72
BR|America/Bahia|-12.98|-38.52
BR|America/Sao_Paulo|-23.53|-46.62
BR|America/Campo_Grande|-20.45|-54.62
BR|America/Cuiaba|-15.58|-56.08
BR|America/Santarem|-2.43|-54.87
BR|America/Porto_Velho|-8.77|-63.9
BR|America/Boa_Vista|2.82|-60.67
BR|America/Manaus|-3.13|-60.02
BR|America/Eirunepe|-6.67|-69.87
BR|America/Rio_Branco|-9.97|-67.8
BS|America/Nassau
BT|Asia/Thimphu
BW|Africa/Gaborone
BY|Europe/Minsk
BZ|America/Belize
CC|Indian/Cocos
CD|Africa/Kinshasa|-4.3|15.3
CD|Africa/Lubumbashi|-11.67|27.47
CF|Africa/Bangui
CG|Africa/Brazzaville
CH|Europe/Zurich
CI|Africa/Abidjan
CK|Pacific/Rarotonga
CL|America/Santiago|-33.45|-70.67
CL|America/Coyhaique|-45.57|-72.07
CL|America/Punta_Arenas|-53.15|-70.92
CL|Pacific/Easter|-27.15|-109.43
CM|Africa/Douala
CN|Asia/Shanghai
CO|America/Bogota
CR|America/Costa_Rica
CU|America/Havana
CV|Atlantic/Cape_Verde
CW|America/Curacao
CX|Indian/Christmas
CY|Asia/Nicosia
CZ|Europe/Prague
DE|Europe/Berlin
DJ|Africa/Djibouti
DK|Europe/Copenhagen
DM|America/Dominica
DO|America/Santo_Domingo
DZ|Africa/Algiers
EC|America/Guayaquil|-2.17|-79.83
EC|Pacific/Galapagos|-0.9|-89.6
EE|Europe/Tallinn
EG|Africa/Cairo
EH|Africa/El_Aaiun
ER|Africa/Asmara
ES|Europe/Madrid|40.4|-3.68
ES|Africa/Ceuta|35.88|-5.32
ES|Atlantic/Canary|28.1|-15.4
ET|Africa/Addis_Ababa
FI|Europe/Helsinki
FJ|Pacific/Fiji
FK|Atlantic/Stanley
FM|Pacific/Chuuk|7.42|151.78
FM|Pacific/Pohnpei|6.97|158.22
FM|Pacific/Kosrae|5.32|162.98
FO|Atlantic/Faroe
FR|Europe/Paris
GA|Africa/Libreville
GB|Europe/London
GD|America/Grenada
GE|Asia/Tbilisi
GF|America/Cayenne
GG|Europe/Guernsey
GH|Africa/Accra
GI|Europe/Gibraltar
GL|America/Nuuk|64.18|-51.73
GL|America/Danmarkshavn|76.77|-18.67
GL|America/Scoresbysund|70.48|-21.97
GL|America/Thule|76.57|-68.78
GM|Africa/Banjul
GN|Africa/Conakry
GP|America/Guadeloupe
GQ|Africa/Malabo
GR|Europe/Athens
GS|Atlantic/South_Georgia
GT|America/Guatemala
GU|Pacific/Guam
GW|Africa/Bissau
GY|America/Guyana
HK|Asia/Hong_Kong
HN|America/Tegucigalpa
HR|Europe/Zagreb
HT|America/Port-au-Prince
HU|Europe/Budapest
ID|Asia/Jakarta|-6.17|106.8
ID|Asia/Pontianak|-0.03|109.33
ID|Asia/Makassar|-5.12|119.4
ID|Asia/Jayapura|-2.53|140.7
IE|Europe/Dublin
IL|Asia/Jerusalem
IM|Europe/Isle_of_Man
IN|Asia/Kolkata
IO|Indian/Chagos
IQ|Asia/Baghdad
IR|Asia/Tehran
IS|Atlantic/Reykjavik
IT|Europe/Rome
JE|Europe/Jersey
JM|America/Jamaica
JO|Asia/Amman
JP|Asia/Tokyo
KE|Africa/Nairobi
KG|Asia/Bishkek
KH|Asia/Phnom_Penh
KI|Pacific/Tarawa|1.42|173
KI|Pacific/Kanton|-2.78|-171.72
KI|Pacific/Kiritimati|1.87|-157.33
KM|Indian/Comoro
KN|America/St_Kitts
KP|Asia/Pyongyang
KR|Asia/Seoul
KW|Asia/Kuwait
KY|America/Cayman
KZ|Asia/Almaty
LA|Asia/Vientiane
LB|Asia/Beirut
LC|America/St_Lucia
LI|Europe/Vaduz
LK|Asia/Colombo
LR|Africa/Monrovia
LS|Africa/Maseru
LT|Europe/Vilnius
LU|Europe/Luxembourg
LV|Europe/Riga
LY|Africa/Tripoli
MA|Africa/Casablanca
MC|Europe/Monaco
MD|Europe/Chisinau
ME|Europe/Podgorica
MF|America/Marigot
MG|Indian/Antananarivo
MH|Pacific/Majuro
MK|Europe/Skopje
ML|Africa/Bamako
MM|Asia/Yangon
MN|Asia/Ulaanbaatar|47.92|106.88
MN|Asia/Hovd|48.02|91.65
MO|Asia/Macau
MP|Pacific/Saipan
MQ|America/Martinique
MR|Africa/Nouakchott
MS|America/Montserrat
MT|Europe/Malta
MU|Indian/Mauritius
MV|Indian/Maldives
MW|Africa/Blantyre
MX|America/Mexico_City|19.4|-99.15
MX|America/Cancun|21.08|-86.77
MX|America/Merida|20.97|-89.62
MX|America/Monterrey|25.67|-100.32
MX|America/Matamoros|25.83|-97.5
MX|America/Chihuahua|28.63|-106.08
MX|America/Ciudad_Juarez|31.73|-106.48
MX|America/Ojinaga|29.57|-104.42
MX|America/Mazatlan|23.22|-106.42
MX|America/Bahia_Banderas|20.8|-105.25
MX|America/Hermosillo|29.07|-110.97
MX|America/Tijuana|32.53|-117.02
MY|Asia/Kuala_Lumpur
MZ|Africa/Maputo
NA|Africa/Windhoek
NC|Pacific/Noumea
NE|Africa/Niamey
NF|Pacific/Norfolk
NG|Africa/Lagos
NI|America/Managua
NL|Europe/Amsterdam
NO|Europe/Oslo
NP|Asia/Kathmandu
NR|Pacific/Nauru
NU|Pacific/Niue
NZ|Pacific/Auckland|-36.87|174.77
NZ|Pacific/Chatham|-43.95|-176.55
OM|Asia/Muscat
PA|America/Panama
PE|America/Lima
PF|Pacific/Tahiti|-17.53|-149.57
PF|Pacific/Marquesas|-9|-139.5
PF|Pacific/Gambier|-23.13|-134.95
PG|Pacific/Port_Moresby|-9.5|147.17
PG|Pacific/Bougainville|-6.22|155.57
PH|Asia/Manila
PK|Asia/Karachi
PL|Europe/Warsaw
PM|America/Miquelon
PN|Pacific/Pitcairn
PR|America/Puerto_Rico
PS|Asia/Gaza
PT|Europe/Lisbon|38.72|-9.13
PT|Atlantic/Madeira|32.63|-16.9
PT|Atlantic/Azores|37.73|-25.67
PW|Pacific/Palau
PY|America/Asuncion
QA|Asia/Qatar
RE|Indian/Reunion
RO|Europe/Bucharest
RS|Europe/Belgrade
RU|Europe/Kaliningrad|54.72|20.5
RU|Europe/Moscow|55.76|37.62
RU|Europe/Kirov|58.6|49.65
RU|Europe/Volgograd|48.73|44.42
RU|Europe/Astrakhan|46.35|48.05
RU|Europe/Saratov|51.57|46.03
RU|Europe/Ulyanovsk|54.33|48.4
RU|Europe/Samara|53.2|50.15
RU|Asia/Yekaterinburg|56.85|60.6
RU|Asia/Omsk|55|73.4
RU|Asia/Novosibirsk|55.03|82.92
RU|Asia/Barnaul|53.37|83.75
RU|Asia/Tomsk|56.5|84.97
RU|Asia/Novokuznetsk|53.75|87.12
RU|Asia/Krasnoyarsk|56.02|92.83
RU|Asia/Irkutsk|52.27|104.33
RU|Asia/Chita|52.05|113.47
RU|Asia/Yakutsk|62|129.67
RU|Asia/Khandyga|62.66|135.55
RU|Asia/Vladivostok|43.17|131.93
RU|Asia/Ust-Nera|64.56|143.23
RU|Asia/Magadan|59.57|150.8
RU|Asia/Sakhalin|46.97|142.7
RU|Asia/Srednekolymsk|67.47|153.72
RU|Asia/Kamchatka|53.02|158.65
RU|Asia/Anadyr|64.75|177.48
RW|Africa/Kigali
SA|Asia/Riyadh
SB|Pacific/Guadalcanal
SC|Indian/Mahe
SD|Africa/Khartoum
SE|Europe/Stockholm
SG|Asia/Singapore
SH|Atlantic/St_Helena
SI|Europe/Ljubljana
SJ|Arctic/Longyearbyen
SK|Europe/Bratislava
SL|Africa/Freetown
SM|Europe/San_Marino
SN|Africa/Dakar
SO|Africa/Mogadishu
SR|America/Paramaribo
SS|Africa/Juba
ST|Africa/Sao_Tome
SV|America/El_Salvador
SX|America/Lower_Princes
SY|Asia/Damascus
SZ|Africa/Mbabane
TC|America/Grand_Turk
TD|Africa/Ndjamena
TF|Indian/Kerguelen
TG|Africa/Lome
TH|Asia/Bangkok
TJ|Asia/Dushanbe
TK|Pacific/Fakaofo
TL|Asia/Dili
TM|Asia/Ashgabat
TN|Africa/Tunis
TO|Pacific/Tongatapu
TR|Europe/Istanbul
TT|America/Port_of_Spain
TV|Pacific/Funafuti
TW|Asia/Taipei
TZ|Africa/Dar_es_Salaam
UA|Europe/Simferopol|44.95|34.1
UA|Europe/Kyiv|50.43|30.52
UG|Africa/Kampala
UM|Pacific/Midway|28.22|-177.37
UM|Pacific/Wake|19.28|166.62
UY|America/Montevideo
UZ|Asia/Samarkand
VA|Europe/Vatican
VC|America/St_Vincent
VE|America/Caracas
VG|America/Tortola
VI|America/St_Thomas
VN|Asia/Ho_Chi_Minh
VU|Pacific/Efate
WF|Pacific/Wallis
WS|Pacific/Apia
YE|Asia/Aden
YT|Indian/Mayotte
ZA|Africa/Johannesburg
ZM|Africa/Lusaka
ZW|Africa/Harare
";

/// `country|state|name|zone` for each state, province or territory of
/// the US, Canada and Australia that keeps one time throughout. Those
/// split between two (Texas, Ontario, New South Wales and others) are
/// left out.
const STATES: &str = "\
US|AL|alabama|America/Chicago
US|AR|arkansas|America/Chicago
US|CA|california|America/Los_Angeles
US|CO|colorado|America/Denver
US|CT|connecticut|America/New_York
US|DC|district of columbia|America/New_York
US|DE|delaware|America/New_York
US|GA|georgia|America/New_York
US|GU|guam|Pacific/Guam
US|HI|hawaii|Pacific/Honolulu
US|IA|iowa|America/Chicago
US|IL|illinois|America/Chicago
US|LA|louisiana|America/Chicago
US|MA|massachusetts|America/New_York
US|MD|maryland|America/New_York
US|ME|maine|America/New_York
US|MN|minnesota|America/Chicago
US|MO|missouri|America/Chicago
US|MS|mississippi|America/Chicago
US|MT|montana|America/Denver
US|NC|north carolina|America/New_York
US|NH|new hampshire|America/New_York
US|NJ|new jersey|America/New_York
US|NM|new mexico|America/Denver
US|NY|new york|America/New_York
US|OH|ohio|America/New_York
US|OK|oklahoma|America/Chicago
US|PA|pennsylvania|America/New_York
US|PR|puerto rico|America/Puerto_Rico
US|RI|rhode island|America/New_York
US|SC|south carolina|America/New_York
US|UT|utah|America/Denver
US|VA|virginia|America/New_York
US|VI|virgin islands|America/St_Thomas
US|VT|vermont|America/New_York
US|WA|washington|America/Los_Angeles
US|WI|wisconsin|America/Chicago
US|WV|west virginia|America/New_York
US|WY|wyoming|America/Denver
CA|AB|alberta|America/Edmonton
CA|MB|manitoba|America/Winnipeg
CA|NB|new brunswick|America/Moncton
CA|NS|nova scotia|America/Halifax
CA|NT|northwest territories|America/Yellowknife
CA|PE|prince edward island|America/Halifax
CA|YT|yukon|America/Whitehorse
AU|ACT|australian capital territory|Australia/Sydney
AU|NT|northern territory|Australia/Darwin
AU|QLD|queensland|Australia/Brisbane
AU|SA|south australia|Australia/Adelaide
AU|TAS|tasmania|Australia/Hobart
AU|VIC|victoria|Australia/Melbourne
AU|WA|western australia|Australia/Perth
";

/// A town takes the clock of the nearest zone only when every zone that
/// keeps another time is this many times as far.
const CLEARLY_NEARER: f64 = 3.0;

/// The clock a town in `country` (a two-letter code) and `region` (as its
/// address gives it), at `lat`, `lon`, keeps, if it can be told.
pub(crate) fn town_zone(
    country: &str,
    region: Option<&str>,
    lat: f64,
    lon: f64,
    now: Timestamp,
) -> Option<TimeZone> {
    if matches!(country, "US" | "CA" | "AU") {
        let region = region?.trim().to_lowercase();
        let zone = STATES.lines().find_map(|line| {
            let mut parts = line.split('|');
            let (state_country, code, name) = (parts.next()?, parts.next()?, parts.next()?);
            let same =
                state_country == country && (code.eq_ignore_ascii_case(&region) || name == region);
            same.then(|| parts.next()).flatten()
        })?;
        return TimeZone::get(zone).ok();
    }
    // Each zone of the country, and how far its city is.
    let zones: Vec<(TimeZone, f64)> = COUNTRIES
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('|');
            (parts.next()? == country).then_some(())?;
            let zone = TimeZone::get(parts.next()?).ok()?;
            let km = match (parts.next(), parts.next()) {
                (Some(at_lat), Some(at_lon)) => {
                    distance_km(lat, lon, at_lat.parse().ok()?, at_lon.parse().ok()?)
                }
                _ => 0.0,
            };
            Some((zone, km))
        })
        .collect();
    let (nearest, km) = zones.iter().min_by(|a, b| a.1.total_cmp(&b.1))?;
    let other = zones
        .iter()
        .filter(|(zone, _)| !same_clock(zone, nearest, now))
        .map(|(_, km)| *km)
        .min_by(f64::total_cmp);
    other
        .is_none_or(|other| other >= CLEARLY_NEARER * km)
        .then(|| nearest.clone())
}

/// Whether `a` and `b` keep the same time over the coming year, week by
/// week.
pub(crate) fn same_clock(a: &TimeZone, b: &TimeZone, now: Timestamp) -> bool {
    (0..53).all(|week| {
        let at = now
            .checked_add(SignedDuration::from_hours(week * 7 * 24))
            .unwrap_or(now);
        a.to_offset(at) == b.to_offset(at)
    })
}

/// Great-circle distance in km.
fn distance_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = (lat2 - lat1).to_radians();
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * 6371.0 * a.sqrt().asin()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-05 03:45:00 UTC.
    const NOW: i64 = 1_791_171_900;

    fn zone(country: &str, region: Option<&str>, lat: f64, lon: f64) -> Option<String> {
        let now = Timestamp::from_second(NOW).unwrap();
        town_zone(country, region, lat, lon, now).map(|zone| zone.iana_name().unwrap().to_string())
    }

    #[test]
    fn every_zone_exists() {
        for line in COUNTRIES.lines().chain(STATES.lines()) {
            let zone = line.split('|').find(|part| part.contains('/')).unwrap();
            assert!(TimeZone::get(zone).is_ok(), "{line}");
        }
    }

    #[test]
    fn a_town_keeps_its_country_or_state_clock() {
        // Cusco, Peru and Boulder, Colorado: one clock throughout.
        assert_eq!(
            zone("PE", None, -13.53, -71.97).as_deref(),
            Some("America/Lima")
        );
        assert_eq!(
            zone("US", Some("CO"), 40.01, -105.27).as_deref(),
            Some("America/Denver")
        );
        assert_eq!(
            zone("AU", Some("Tasmania"), -42.88, 147.33).as_deref(),
            Some("Australia/Hobart")
        );
        // Texas and Ontario keep two; a town there is not guessed.
        assert_eq!(zone("US", Some("TX"), 31.76, -106.49), None);
        assert_eq!(zone("CA", Some("ON"), 42.98, -81.25), None);
        assert_eq!(zone("US", None, 40.01, -105.27), None);
        // Porto is far nearer Lisbon than the Azores.
        assert_eq!(
            zone("PT", None, 41.15, -8.61).as_deref(),
            Some("Europe/Lisbon")
        );
        assert_eq!(
            zone("PT", None, 37.74, -25.68).as_deref(),
            Some("Atlantic/Azores")
        );
        // Izhevsk is nearer Kirov (Moscow time) than Samara, whose time it
        // keeps: too close to tell, so no answer.
        assert_eq!(zone("RU", None, 56.85, 53.2), None);
        assert_eq!(zone("XX", None, 0.0, 0.0), None);
    }
}

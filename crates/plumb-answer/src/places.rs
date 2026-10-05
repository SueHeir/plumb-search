//! Places people ask the time in, and the time zone each is in: cities,
//! countries (one zone each; a country with several gets its capital's),
//! US states, Canadian provinces, Australian states, and the names and
//! abbreviations of zones ("pacific", "est").

/// `name|shown as|zone`, one a line; names are lowercase.
const PLACES: &str = "\
abu dhabi|Abu Dhabi, United Arab Emirates|Asia/Dubai
accra|Accra, Ghana|Africa/Accra
addis ababa|Addis Ababa, Ethiopia|Africa/Addis_Ababa
adelaide|Adelaide, Australia|Australia/Adelaide
ahmedabad|Ahmedabad, India|Asia/Kolkata
albuquerque|Albuquerque, US|America/Denver
algiers|Algiers, Algeria|Africa/Algiers
almaty|Almaty, Kazakhstan|Asia/Almaty
amman|Amman, Jordan|Asia/Amman
amsterdam|Amsterdam, Netherlands|Europe/Amsterdam
anchorage|Anchorage, US|America/Anchorage
ankara|Ankara, Turkey|Europe/Istanbul
athens|Athens, Greece|Europe/Athens
atlanta|Atlanta, US|America/New_York
auckland|Auckland, New Zealand|Pacific/Auckland
austin|Austin, US|America/Chicago
baghdad|Baghdad, Iraq|Asia/Baghdad
baku|Baku, Azerbaijan|Asia/Baku
baltimore|Baltimore, US|America/New_York
bangalore|Bengaluru, India|Asia/Kolkata
bengaluru|Bengaluru, India|Asia/Kolkata
bangkok|Bangkok, Thailand|Asia/Bangkok
barcelona|Barcelona, Spain|Europe/Madrid
beijing|Beijing, China|Asia/Shanghai
beirut|Beirut, Lebanon|Asia/Beirut
belgrade|Belgrade, Serbia|Europe/Belgrade
berlin|Berlin, Germany|Europe/Berlin
bern|Bern, Switzerland|Europe/Zurich
bogota|Bogotá, Colombia|America/Bogota
bogotá|Bogotá, Colombia|America/Bogota
boise|Boise, US|America/Boise
boston|Boston, US|America/New_York
brasilia|Brasília, Brazil|America/Sao_Paulo
brasília|Brasília, Brazil|America/Sao_Paulo
bratislava|Bratislava, Slovakia|Europe/Bratislava
brisbane|Brisbane, Australia|Australia/Brisbane
brussels|Brussels, Belgium|Europe/Brussels
bucharest|Bucharest, Romania|Europe/Bucharest
budapest|Budapest, Hungary|Europe/Budapest
buenos aires|Buenos Aires, Argentina|America/Argentina/Buenos_Aires
cairo|Cairo, Egypt|Africa/Cairo
calgary|Calgary, Canada|America/Edmonton
canberra|Canberra, Australia|Australia/Sydney
cape town|Cape Town, South Africa|Africa/Johannesburg
caracas|Caracas, Venezuela|America/Caracas
casablanca|Casablanca, Morocco|Africa/Casablanca
charlotte|Charlotte, US|America/New_York
chennai|Chennai, India|Asia/Kolkata
chicago|Chicago, US|America/Chicago
chongqing|Chongqing, China|Asia/Shanghai
cincinnati|Cincinnati, US|America/New_York
cleveland|Cleveland, US|America/New_York
colombo|Colombo, Sri Lanka|Asia/Colombo
columbus|Columbus, US|America/New_York
copenhagen|Copenhagen, Denmark|Europe/Copenhagen
dakar|Dakar, Senegal|Africa/Dakar
dallas|Dallas, US|America/Chicago
damascus|Damascus, Syria|Asia/Damascus
dar es salaam|Dar es Salaam, Tanzania|Africa/Dar_es_Salaam
darwin|Darwin, Australia|Australia/Darwin
delhi|Delhi, India|Asia/Kolkata
new delhi|New Delhi, India|Asia/Kolkata
denver|Denver, US|America/Denver
detroit|Detroit, US|America/Detroit
dhaka|Dhaka, Bangladesh|Asia/Dhaka
doha|Doha, Qatar|Asia/Qatar
dubai|Dubai, United Arab Emirates|Asia/Dubai
dublin|Dublin, Ireland|Europe/Dublin
edinburgh|Edinburgh, United Kingdom|Europe/London
edmonton|Edmonton, Canada|America/Edmonton
frankfurt|Frankfurt, Germany|Europe/Berlin
geneva|Geneva, Switzerland|Europe/Zurich
guangzhou|Guangzhou, China|Asia/Shanghai
guatemala city|Guatemala City, Guatemala|America/Guatemala
halifax|Halifax, Canada|America/Halifax
hamburg|Hamburg, Germany|Europe/Berlin
hanoi|Hanoi, Vietnam|Asia/Ho_Chi_Minh
havana|Havana, Cuba|America/Havana
helsinki|Helsinki, Finland|Europe/Helsinki
ho chi minh city|Ho Chi Minh City, Vietnam|Asia/Ho_Chi_Minh
saigon|Ho Chi Minh City, Vietnam|Asia/Ho_Chi_Minh
hong kong|Hong Kong|Asia/Hong_Kong
honolulu|Honolulu, US|Pacific/Honolulu
houston|Houston, US|America/Chicago
hyderabad|Hyderabad, India|Asia/Kolkata
indianapolis|Indianapolis, US|America/Indiana/Indianapolis
islamabad|Islamabad, Pakistan|Asia/Karachi
istanbul|Istanbul, Turkey|Europe/Istanbul
jakarta|Jakarta, Indonesia|Asia/Jakarta
jeddah|Jeddah, Saudi Arabia|Asia/Riyadh
jerusalem|Jerusalem|Asia/Jerusalem
johannesburg|Johannesburg, South Africa|Africa/Johannesburg
kabul|Kabul, Afghanistan|Asia/Kabul
kansas city|Kansas City, US|America/Chicago
karachi|Karachi, Pakistan|Asia/Karachi
kathmandu|Kathmandu, Nepal|Asia/Kathmandu
kyiv|Kyiv, Ukraine|Europe/Kyiv
kiev|Kyiv, Ukraine|Europe/Kyiv
kolkata|Kolkata, India|Asia/Kolkata
calcutta|Kolkata, India|Asia/Kolkata
kuala lumpur|Kuala Lumpur, Malaysia|Asia/Kuala_Lumpur
kuwait city|Kuwait City, Kuwait|Asia/Kuwait
kyoto|Kyoto, Japan|Asia/Tokyo
lagos|Lagos, Nigeria|Africa/Lagos
lahore|Lahore, Pakistan|Asia/Karachi
las vegas|Las Vegas, US|America/Los_Angeles
lima|Lima, Peru|America/Lima
lisbon|Lisbon, Portugal|Europe/Lisbon
ljubljana|Ljubljana, Slovenia|Europe/Ljubljana
london|London, United Kingdom|Europe/London
los angeles|Los Angeles, US|America/Los_Angeles
la|Los Angeles, US|America/Los_Angeles
luxembourg|Luxembourg|Europe/Luxembourg
lyon|Lyon, France|Europe/Paris
madrid|Madrid, Spain|Europe/Madrid
manchester|Manchester, United Kingdom|Europe/London
manila|Manila, Philippines|Asia/Manila
marrakech|Marrakesh, Morocco|Africa/Casablanca
mecca|Mecca, Saudi Arabia|Asia/Riyadh
melbourne|Melbourne, Australia|Australia/Melbourne
mexico city|Mexico City, Mexico|America/Mexico_City
miami|Miami, US|America/New_York
milan|Milan, Italy|Europe/Rome
minneapolis|Minneapolis, US|America/Chicago
minsk|Minsk, Belarus|Europe/Minsk
monaco|Monaco|Europe/Monaco
montevideo|Montevideo, Uruguay|America/Montevideo
montreal|Montreal, Canada|America/Toronto
montréal|Montreal, Canada|America/Toronto
moscow|Moscow, Russia|Europe/Moscow
mumbai|Mumbai, India|Asia/Kolkata
bombay|Mumbai, India|Asia/Kolkata
munich|Munich, Germany|Europe/Berlin
muscat|Muscat, Oman|Asia/Muscat
nairobi|Nairobi, Kenya|Africa/Nairobi
nashville|Nashville, US|America/Chicago
new orleans|New Orleans, US|America/Chicago
new york|New York, US|America/New_York
new york city|New York, US|America/New_York
nyc|New York, US|America/New_York
nice|Nice, France|Europe/Paris
osaka|Osaka, Japan|Asia/Tokyo
oslo|Oslo, Norway|Europe/Oslo
ottawa|Ottawa, Canada|America/Toronto
panama city|Panama City, Panama|America/Panama
paris|Paris, France|Europe/Paris
perth|Perth, Australia|Australia/Perth
philadelphia|Philadelphia, US|America/New_York
phoenix|Phoenix, US|America/Phoenix
pittsburgh|Pittsburgh, US|America/New_York
portland|Portland, US|America/Los_Angeles
prague|Prague, Czechia|Europe/Prague
quebec city|Quebec City, Canada|America/Toronto
quito|Quito, Ecuador|America/Guayaquil
reykjavik|Reykjavík, Iceland|Atlantic/Reykjavik
reykjavík|Reykjavík, Iceland|Atlantic/Reykjavik
riga|Riga, Latvia|Europe/Riga
rio de janeiro|Rio de Janeiro, Brazil|America/Sao_Paulo
rio|Rio de Janeiro, Brazil|America/Sao_Paulo
riyadh|Riyadh, Saudi Arabia|Asia/Riyadh
rome|Rome, Italy|Europe/Rome
salt lake city|Salt Lake City, US|America/Denver
san antonio|San Antonio, US|America/Chicago
san diego|San Diego, US|America/Los_Angeles
san francisco|San Francisco, US|America/Los_Angeles
sf|San Francisco, US|America/Los_Angeles
san jose|San Jose, US|America/Los_Angeles
san juan|San Juan, Puerto Rico|America/Puerto_Rico
santiago|Santiago, Chile|America/Santiago
sao paulo|São Paulo, Brazil|America/Sao_Paulo
são paulo|São Paulo, Brazil|America/Sao_Paulo
seattle|Seattle, US|America/Los_Angeles
seoul|Seoul, South Korea|Asia/Seoul
shanghai|Shanghai, China|Asia/Shanghai
shenzhen|Shenzhen, China|Asia/Shanghai
silicon valley|San Jose, US|America/Los_Angeles
singapore|Singapore|Asia/Singapore
sofia|Sofia, Bulgaria|Europe/Sofia
st louis|St. Louis, US|America/Chicago
st. louis|St. Louis, US|America/Chicago
st petersburg|Saint Petersburg, Russia|Europe/Moscow
saint petersburg|Saint Petersburg, Russia|Europe/Moscow
stockholm|Stockholm, Sweden|Europe/Stockholm
sydney|Sydney, Australia|Australia/Sydney
taipei|Taipei, Taiwan|Asia/Taipei
tallinn|Tallinn, Estonia|Europe/Tallinn
tashkent|Tashkent, Uzbekistan|Asia/Tashkent
tbilisi|Tbilisi, Georgia|Asia/Tbilisi
tehran|Tehran, Iran|Asia/Tehran
tel aviv|Tel Aviv, Israel|Asia/Jerusalem
tokyo|Tokyo, Japan|Asia/Tokyo
toronto|Toronto, Canada|America/Toronto
tunis|Tunis, Tunisia|Africa/Tunis
ulaanbaatar|Ulaanbaatar, Mongolia|Asia/Ulaanbaatar
vancouver|Vancouver, Canada|America/Vancouver
venice|Venice, Italy|Europe/Rome
vienna|Vienna, Austria|Europe/Vienna
vilnius|Vilnius, Lithuania|Europe/Vilnius
warsaw|Warsaw, Poland|Europe/Warsaw
washington dc|Washington, D.C., US|America/New_York
washington d.c.|Washington, D.C., US|America/New_York
dc|Washington, D.C., US|America/New_York
wellington|Wellington, New Zealand|Pacific/Auckland
winnipeg|Winnipeg, Canada|America/Winnipeg
yangon|Yangon, Myanmar|Asia/Yangon
zagreb|Zagreb, Croatia|Europe/Zagreb
zurich|Zurich, Switzerland|Europe/Zurich
zürich|Zurich, Switzerland|Europe/Zurich
afghanistan|Afghanistan|Asia/Kabul
albania|Albania|Europe/Tirane
algeria|Algeria|Africa/Algiers
argentina|Argentina|America/Argentina/Buenos_Aires
armenia|Armenia|Asia/Yerevan
australia|Canberra, Australia|Australia/Sydney
austria|Austria|Europe/Vienna
azerbaijan|Azerbaijan|Asia/Baku
bahrain|Bahrain|Asia/Bahrain
bangladesh|Bangladesh|Asia/Dhaka
belarus|Belarus|Europe/Minsk
belgium|Belgium|Europe/Brussels
bolivia|Bolivia|America/La_Paz
bosnia|Bosnia and Herzegovina|Europe/Sarajevo
brazil|Brasília, Brazil|America/Sao_Paulo
bulgaria|Bulgaria|Europe/Sofia
cambodia|Cambodia|Asia/Phnom_Penh
cameroon|Cameroon|Africa/Douala
canada|Ottawa, Canada|America/Toronto
chile|Chile|America/Santiago
china|China|Asia/Shanghai
colombia|Colombia|America/Bogota
costa rica|Costa Rica|America/Costa_Rica
croatia|Croatia|Europe/Zagreb
cuba|Cuba|America/Havana
cyprus|Cyprus|Asia/Nicosia
czechia|Czechia|Europe/Prague
czech republic|Czechia|Europe/Prague
denmark|Denmark|Europe/Copenhagen
dominican republic|Dominican Republic|America/Santo_Domingo
ecuador|Ecuador|America/Guayaquil
egypt|Egypt|Africa/Cairo
el salvador|El Salvador|America/El_Salvador
england|United Kingdom|Europe/London
estonia|Estonia|Europe/Tallinn
ethiopia|Ethiopia|Africa/Addis_Ababa
finland|Finland|Europe/Helsinki
france|France|Europe/Paris
georgia|Georgia, US|America/New_York
germany|Germany|Europe/Berlin
ghana|Ghana|Africa/Accra
greece|Greece|Europe/Athens
guatemala|Guatemala|America/Guatemala
honduras|Honduras|America/Tegucigalpa
hungary|Hungary|Europe/Budapest
iceland|Iceland|Atlantic/Reykjavik
india|India|Asia/Kolkata
indonesia|Jakarta, Indonesia|Asia/Jakarta
iran|Iran|Asia/Tehran
iraq|Iraq|Asia/Baghdad
ireland|Ireland|Europe/Dublin
israel|Israel|Asia/Jerusalem
italy|Italy|Europe/Rome
jamaica|Jamaica|America/Jamaica
japan|Japan|Asia/Tokyo
jordan|Jordan|Asia/Amman
kazakhstan|Astana, Kazakhstan|Asia/Almaty
kenya|Kenya|Africa/Nairobi
kuwait|Kuwait|Asia/Kuwait
latvia|Latvia|Europe/Riga
lebanon|Lebanon|Asia/Beirut
lithuania|Lithuania|Europe/Vilnius
malaysia|Malaysia|Asia/Kuala_Lumpur
malta|Malta|Europe/Malta
mexico|Mexico City, Mexico|America/Mexico_City
moldova|Moldova|Europe/Chisinau
mongolia|Ulaanbaatar, Mongolia|Asia/Ulaanbaatar
morocco|Morocco|Africa/Casablanca
myanmar|Myanmar|Asia/Yangon
nepal|Nepal|Asia/Kathmandu
netherlands|Netherlands|Europe/Amsterdam
holland|Netherlands|Europe/Amsterdam
new zealand|New Zealand|Pacific/Auckland
nicaragua|Nicaragua|America/Managua
nigeria|Nigeria|Africa/Lagos
north korea|North Korea|Asia/Pyongyang
norway|Norway|Europe/Oslo
oman|Oman|Asia/Muscat
pakistan|Pakistan|Asia/Karachi
panama|Panama|America/Panama
paraguay|Paraguay|America/Asuncion
peru|Peru|America/Lima
philippines|Philippines|Asia/Manila
poland|Poland|Europe/Warsaw
portugal|Lisbon, Portugal|Europe/Lisbon
puerto rico|Puerto Rico|America/Puerto_Rico
qatar|Qatar|Asia/Qatar
romania|Romania|Europe/Bucharest
russia|Moscow, Russia|Europe/Moscow
saudi arabia|Saudi Arabia|Asia/Riyadh
scotland|United Kingdom|Europe/London
senegal|Senegal|Africa/Dakar
serbia|Serbia|Europe/Belgrade
slovakia|Slovakia|Europe/Bratislava
slovenia|Slovenia|Europe/Ljubljana
south africa|South Africa|Africa/Johannesburg
south korea|South Korea|Asia/Seoul
korea|South Korea|Asia/Seoul
spain|Madrid, Spain|Europe/Madrid
sri lanka|Sri Lanka|Asia/Colombo
sweden|Sweden|Europe/Stockholm
switzerland|Switzerland|Europe/Zurich
syria|Syria|Asia/Damascus
taiwan|Taiwan|Asia/Taipei
tanzania|Tanzania|Africa/Dar_es_Salaam
thailand|Thailand|Asia/Bangkok
tunisia|Tunisia|Africa/Tunis
turkey|Turkey|Europe/Istanbul
türkiye|Turkey|Europe/Istanbul
uae|United Arab Emirates|Asia/Dubai
united arab emirates|United Arab Emirates|Asia/Dubai
uganda|Uganda|Africa/Kampala
uk|United Kingdom|Europe/London
united kingdom|United Kingdom|Europe/London
great britain|United Kingdom|Europe/London
britain|United Kingdom|Europe/London
wales|United Kingdom|Europe/London
ukraine|Ukraine|Europe/Kyiv
uruguay|Uruguay|America/Montevideo
us|Washington, D.C., US|America/New_York
usa|Washington, D.C., US|America/New_York
united states|Washington, D.C., US|America/New_York
america|Washington, D.C., US|America/New_York
uzbekistan|Uzbekistan|Asia/Tashkent
venezuela|Venezuela|America/Caracas
vietnam|Vietnam|Asia/Ho_Chi_Minh
yemen|Yemen|Asia/Aden
zambia|Zambia|Africa/Lusaka
zimbabwe|Zimbabwe|Africa/Harare
alabama|Alabama, US|America/Chicago
alaska|Alaska, US|America/Anchorage
arizona|Arizona, US|America/Phoenix
arkansas|Arkansas, US|America/Chicago
california|California, US|America/Los_Angeles
colorado|Colorado, US|America/Denver
connecticut|Connecticut, US|America/New_York
delaware|Delaware, US|America/New_York
florida|Florida, US|America/New_York
hawaii|Hawaii, US|Pacific/Honolulu
idaho|Boise, Idaho, US|America/Boise
illinois|Illinois, US|America/Chicago
indiana|Indiana, US|America/Indiana/Indianapolis
iowa|Iowa, US|America/Chicago
kansas|Kansas, US|America/Chicago
kentucky|Kentucky, US|America/New_York
louisiana|Louisiana, US|America/Chicago
maine|Maine, US|America/New_York
maryland|Maryland, US|America/New_York
massachusetts|Massachusetts, US|America/New_York
michigan|Michigan, US|America/Detroit
minnesota|Minnesota, US|America/Chicago
mississippi|Mississippi, US|America/Chicago
missouri|Missouri, US|America/Chicago
montana|Montana, US|America/Denver
nebraska|Nebraska, US|America/Chicago
nevada|Nevada, US|America/Los_Angeles
new hampshire|New Hampshire, US|America/New_York
new jersey|New Jersey, US|America/New_York
new mexico|New Mexico, US|America/Denver
new york state|New York, US|America/New_York
north carolina|North Carolina, US|America/New_York
north dakota|North Dakota, US|America/Chicago
ohio|Ohio, US|America/New_York
oklahoma|Oklahoma, US|America/Chicago
oregon|Oregon, US|America/Los_Angeles
pennsylvania|Pennsylvania, US|America/New_York
rhode island|Rhode Island, US|America/New_York
south carolina|South Carolina, US|America/New_York
south dakota|South Dakota, US|America/Chicago
tennessee|Nashville, Tennessee, US|America/Chicago
texas|Texas, US|America/Chicago
utah|Utah, US|America/Denver
vermont|Vermont, US|America/New_York
virginia|Virginia, US|America/New_York
washington|Washington, US|America/Los_Angeles
washington state|Washington, US|America/Los_Angeles
west virginia|West Virginia, US|America/New_York
wisconsin|Wisconsin, US|America/Chicago
wyoming|Wyoming, US|America/Denver
alberta|Alberta, Canada|America/Edmonton
british columbia|British Columbia, Canada|America/Vancouver
manitoba|Manitoba, Canada|America/Winnipeg
new brunswick|New Brunswick, Canada|America/Moncton
newfoundland|Newfoundland, Canada|America/St_Johns
nova scotia|Nova Scotia, Canada|America/Halifax
ontario|Ontario, Canada|America/Toronto
quebec|Quebec, Canada|America/Toronto
saskatchewan|Saskatchewan, Canada|America/Regina
new south wales|New South Wales, Australia|Australia/Sydney
victoria|Victoria, Australia|Australia/Melbourne
queensland|Queensland, Australia|Australia/Brisbane
south australia|South Australia|Australia/Adelaide
western australia|Western Australia|Australia/Perth
tasmania|Tasmania, Australia|Australia/Hobart
northern territory|Northern Territory, Australia|Australia/Darwin
eastern|Eastern Time (US)|America/New_York
eastern time|Eastern Time (US)|America/New_York
est|Eastern Time (US)|America/New_York
edt|Eastern Time (US)|America/New_York
et|Eastern Time (US)|America/New_York
central|Central Time (US)|America/Chicago
central time|Central Time (US)|America/Chicago
cst|Central Time (US)|America/Chicago
cdt|Central Time (US)|America/Chicago
ct|Central Time (US)|America/Chicago
mountain|Mountain Time (US)|America/Denver
mountain time|Mountain Time (US)|America/Denver
mst|Mountain Time (US)|America/Denver
mdt|Mountain Time (US)|America/Denver
mt|Mountain Time (US)|America/Denver
pacific|Pacific Time (US)|America/Los_Angeles
pacific time|Pacific Time (US)|America/Los_Angeles
pst|Pacific Time (US)|America/Los_Angeles
pdt|Pacific Time (US)|America/Los_Angeles
pt|Pacific Time (US)|America/Los_Angeles
akst|Alaska Time|America/Anchorage
akdt|Alaska Time|America/Anchorage
hst|Hawaii Time|Pacific/Honolulu
atlantic|Atlantic Time (Canada)|America/Halifax
ast|Atlantic Time (Canada)|America/Halifax
gmt|Greenwich Mean Time|Etc/GMT
utc|Coordinated Universal Time|UTC
zulu|Coordinated Universal Time|UTC
bst|United Kingdom|Europe/London
cet|Central European Time|Europe/Paris
cest|Central European Time|Europe/Paris
eet|Eastern European Time|Europe/Athens
eest|Eastern European Time|Europe/Athens
wet|Western European Time|Europe/Lisbon
ist|India|Asia/Kolkata
jst|Japan|Asia/Tokyo
kst|South Korea|Asia/Seoul
hkt|Hong Kong|Asia/Hong_Kong
sgt|Singapore|Asia/Singapore
aest|Eastern Australia|Australia/Sydney
aedt|Eastern Australia|Australia/Sydney
acst|Central Australia|Australia/Adelaide
awst|Western Australia|Australia/Perth
nzst|New Zealand|Pacific/Auckland
nzdt|New Zealand|Pacific/Auckland
msk|Moscow, Russia|Europe/Moscow
";

/// The place `name` (lowercase) names, as shown, and its zone.
pub(crate) fn find(name: &str) -> Option<(&'static str, &'static str)> {
    PLACES.lines().find_map(|line| {
        let mut parts = line.split('|');
        (parts.next()? == name).then_some(())?;
        Some((parts.next()?, parts.next()?))
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_zone_exists() {
        for line in super::PLACES.lines() {
            let zone = line.rsplit('|').next().unwrap();
            assert_eq!(line.split('|').count(), 3, "{line}");
            assert!(jiff::tz::TimeZone::get(zone).is_ok(), "{line}");
            assert_eq!(
                line.split('|').next().unwrap(),
                line.split('|').next().unwrap().to_lowercase()
            );
        }
    }
}

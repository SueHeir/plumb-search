//! The language of a text in Latin letters, told by its common words, for
//! a site that does not declare its own ([`crate::site_language`]).

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

/// Words nearly every sentence of a language has and other listed
/// languages rarely use, so one alone points to it: not "en", "de" or "con",
/// which several languages share, nor "do" or "per", which are English too.
/// Nor words often found in English names: "los" and "las" (Los Angeles,
/// Las Vegas), "nuevo" (Nuevo Laredo), "chez" (Chez Panisse), "est" (Est.
/// 1887).
const COMMON_WORDS: &[(&str, &str)] = &[
    (
        "en",
        "the and of with your our you from this that are be by more about how what have has their \
         they which when where welcome official free find",
    ),
    (
        "es",
        "y más sus nuestro nuestra nuestros nuestras también usted encuentra servicios productos \
         mejor mejores noticias información inicio bienvenidos tus hoy muy días años aquí ahora \
         cómo qué ofrecemos precios tienda pero puedes podrás descubre hacer están tiempo envío",
    ),
    (
        "pt",
        "em não seu sua ao pelo pela você nosso nossa também seus suas serviços produtos melhor \
         notícias informações início uma muito maior fazer confira",
    ),
    (
        "fr",
        "les et pour une avec sur aux dans vous nous votre vos notre cette sont tous toutes \
         découvrez accueil bienvenue entreprise produits meilleur nouveau actualités depuis très \
         être également trouvez retrouvez réservez ligne",
    ),
    (
        "de",
        "der und für mit von zu ist ein eine einen auf im bei sie wir ihr ihre nicht auch zum zur \
         über aus oder sind unsere unser ihren sich wird werden startseite willkommen unternehmen \
         angebote",
    ),
    (
        "it",
        "della dei delle degli che sono nel nella gli più anche tutti tutte tuo tua nostro nostra \
         nostri sito azienda prodotti migliori notizie benvenuti scopri questo oggi molto",
    ),
    (
        "nl",
        "het een voor op zijn bij ons onze ook naar uit aan jouw wij welkom bedrijf producten \
         nieuws niet deze heeft wordt",
    ),
    ("sv", "och för att ett är inte våra vår välkommen företag"),
    ("pl", "się jest dla od oraz że jak przez są witamy strona"),
    (
        "tr",
        "ve bir ile için çok olarak daha veya hakkında anasayfa tüm büyük yeni nasıl",
    ),
    (
        "id",
        "yang untuk dengan ini dari pada dalam kami anda atau adalah",
    ),
    ("vi", "và các có trong nhà"),
    ("cs", "jsou jsme který která které také nebo však naše vaše"),
    ("hu", "és egy hogy vagy rólunk kapcsolat oldal"),
    ("ro", "pentru în sunt despre acasă şi"),
];

/// Letters only one listed language writes, so a word holding one counts
/// as one of that language's common words: "Việt", "Bankası", "Wrocław",
/// "pořídíte", "főoldal", "Constanța". A name or two ("Bogusław Kaczyński,
/// Polish journalist") is still too few. Not "ş", which Romanian sites
/// write for "ș" too, nor "ć", which Croatian writes.
const LETTERS: &[(&str, &str)] = &[
    ("vi", "ơưạảấầẩẫậắằẳẵặẹẻẽếềểễệỉịọỏốồổỗộớờởỡợụủứừửữựỳỵỷỹ"),
    ("tr", "ığ"),
    ("pl", "łśźżąęń"),
    ("cs", "řůě"),
    ("hu", "őű"),
    ("ro", "șțţ"),
];

/// Fewest common words of a language that tell it.
const MIN_WORDS: usize = 3;

/// Each common word with the index of its language in [`COMMON_WORDS`].
fn common_words() -> &'static HashMap<&'static str, usize> {
    static WORDS: OnceLock<HashMap<&'static str, usize>> = OnceLock::new();
    WORDS.get_or_init(|| {
        COMMON_WORDS
            .iter()
            .enumerate()
            .flat_map(|(i, (_, words))| words.split_whitespace().map(move |word| (word, i)))
            .collect()
    })
}

/// Each of [`LETTERS`] with the index of its language in [`COMMON_WORDS`].
fn letters() -> &'static HashMap<char, usize> {
    static LETTERS_OF: OnceLock<HashMap<char, usize>> = OnceLock::new();
    LETTERS_OF.get_or_init(|| {
        LETTERS
            .iter()
            .flat_map(|&(language, letters)| {
                let i = COMMON_WORDS.iter().position(|&(code, _)| code == language);
                letters.chars().filter_map(move |letter| Some((letter, i?)))
            })
            .collect()
    })
}

/// Whether [`language_by_words`] can tell `language`: a site it takes for
/// German may be Danish, so it says nothing about languages not listed.
pub(crate) fn tells(language: &str) -> bool {
    COMMON_WORDS.iter().any(|(code, _)| *code == language)
}

/// The language `text` is in, when its [`COMMON_WORDS`] and words with its
/// [`LETTERS`] say so clearly: [`MIN_WORDS`] or more different words of one
/// language's, and more than twice as many as of any other. `None` for a
/// short text, a name or a mix ("Hotel Casa del Mar | Beachfront rooms"),
/// which say too little. A word keeps its apostrophe, so "we've" is not the
/// Turkish "ve", and one in capitals ("IL", "ET", "AU") is an abbreviation,
/// not counted.
pub(crate) fn language_by_words(text: &str) -> Option<&'static str> {
    let found: HashSet<String> = text
        .split(|c: char| !(c.is_alphabetic() || is_apostrophe(c)))
        .map(|word| word.trim_matches(is_apostrophe))
        .filter(|word| word.chars().nth(1).is_none() || word.chars().any(char::is_lowercase))
        .map(str::to_lowercase)
        .collect();
    let mut counts = [0; COMMON_WORDS.len()];
    for word in &found {
        let language = common_words()
            .get(word.as_str())
            .or_else(|| word.chars().find_map(|letter| letters().get(&letter)));
        if let Some(&i) = language {
            counts[i] += 1;
        }
    }
    let (best, &most) = counts.iter().enumerate().max_by_key(|&(_, n)| n)?;
    let next = counts
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != best)
        .map(|(_, &n)| n)
        .max()
        .unwrap_or(0);
    (most >= MIN_WORDS && most > 2 * next).then_some(COMMON_WORDS[best].0)
}

fn is_apostrophe(c: char) -> bool {
    matches!(c, '\'' | '\u{2019}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_word_or_letter_is_common_to_two_languages() {
        let listed: usize = COMMON_WORDS
            .iter()
            .map(|(_, words)| words.split_whitespace().count())
            .sum();
        assert_eq!(common_words().len(), listed);
        let letters_listed: usize = LETTERS
            .iter()
            .map(|(_, letters)| letters.chars().count())
            .sum();
        assert_eq!(letters().len(), letters_listed);
    }

    #[test]
    fn languages_are_told_by_their_common_words() {
        for (text, language) in [
            (
                "Noticias Cancún - Noticias Locales, Nacionales e Internacionales. \
                 El clima en Mexico y el Mundo para hoy y los próximos días",
                "es",
            ),
            (
                "Promesse de Fleurs, plantes de jardin de qualité depuis 1950. Promesse de \
                 fleurs, plantes pour le jardin : vivaces, arbustes et rosiers avec nos conseils",
                "fr",
            ),
            (
                "Startseite - Toskanaworld. Willkommen bei Toskanaworld! Wir bieten magische \
                 Orte der Heilung und Entspannung",
                "de",
            ),
            (
                "Brazil. O gov.br reúne, em um só lugar, serviços para o cidadão e \
                 informações sobre a atuação de todas as áreas do governo",
                "pt",
            ),
            (
                "Ministry of Labour and Social Solidarity. The official website of the \
                 ministry, with information about its services",
                "en",
            ),
            (
                "TOM Bank | Türkiye'nin En Büyük Dijital Bankası. Türkiye’nin en büyük \
                 dijital bankası ile bir çok hizmet için",
                "tr",
            ),
            // Told by their letters.
            ("Báo VnExpress - Báo tiếng Việt nhiều người xem nhất", "vi"),
            (
                "Türk Ekonomi Bankası TEB ile ihtiyaçlarınıza uygun kredi, kredi kartı",
                "tr",
            ),
            (
                "Vítejte na stránkách města Brna. Najdete zde aktuality, úřední desku a \
                 služby, které město nabízí",
                "cs",
            ),
            (
                "Főoldal - Az Országgyűlés honlapja, hírek és információk egy helyen",
                "hu",
            ),
            (
                "Primăria Municipiului București - Știri și informații pentru cetățeni",
                "ro",
            ),
            (
                "Domena.pl | Domeny, hosting, rejestracja domen. Firma świadczy usługi \
                 rejestracji domen, hostingu i poczty, które są tanie",
                "pl",
            ),
        ] {
            assert_eq!(language_by_words(text), Some(language), "{text}");
        }
        for text in [
            "",
            "Skyscanner",
            "Hotel Casa del Mar | Beachfront rooms",
            "Los Angeles Times",
            // A French title over an English description.
            "Le Monde: actualités et toutes les infos | The news of the world",
            // One word three times, or English names and abbreviations.
            "Las Vegas Hotels | Las Vegas Shows & Las Vegas Deals",
            "Naperville, IL Dentist | Family Dentistry in Naperville, IL - Aurora, IL",
            "Best NAS for Home | NAS Drives, NAS OS Reviews",
            "AUS vs IND Live Score | IND vs AUS 3rd Test | AUS tour of India",
            "Los Angeles to Las Vegas | El Paso to Los Alamos",
            "We've got it, you've seen it, they've loved it",
            "Smith et al. 2019 | Notre Dame Journal of Formal Logic | Est. 1960",
            "Pour House Bar & Grill | Est. 2005 | Live Music Fri 9 PM ET",
            "Bar Pas Mal | Wine Bar Est. 2015 | Chez Nous Bistro",
            "Taqueria Siempre Muy Bueno | Nuevo Tacos",
            // A name or two with their own letters, in English.
            "Bogusław Kaczyński Polish classical music journalist (1942–2016)",
            "Săvinești commune in Neamț County, Romania",
            "Phở Bắc | Vietnamese Restaurant in Seattle",
            "Erdoğan visits Kadıköy and Beşiktaş",
        ] {
            assert_eq!(language_by_words(text), None, "{text}");
        }
    }
}

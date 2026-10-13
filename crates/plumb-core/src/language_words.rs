//! The language of a text in Latin letters, told by its common words, for
//! a site that does not declare its own ([`crate::site_language`]).

use std::collections::HashMap;
use std::sync::OnceLock;

/// Words nearly every sentence of a language has and no other listed
/// language uses, so one alone points to it: not "en", "de" or "con",
/// which several languages share, nor "do" or "per", which are English too.
const COMMON_WORDS: &[(&str, &str)] = &[
    (
        "en",
        "the and of with your our you from this that are be by more about how what have has \
         their they which when where welcome official free find",
    ),
    (
        "es",
        "el los las y más sus nuestro nuestra nuestros también usted encuentra servicios \
         productos mejor mejores nuevo nueva noticias información inicio bienvenidos tus",
    ),
    (
        "pt",
        "os em nas não seu sua ao pelo pela você nosso nossa também seus suas serviços \
         produtos melhor notícias informações início uma",
    ),
    (
        "fr",
        "les et pour une est avec sur au aux dans vous nous votre vos notre cette qui sont ou \
         pas tous toutes chez découvrez accueil bienvenue entreprise produits meilleur \
         nouveau actualités",
    ),
    (
        "de",
        "der und für mit von zu dem ist ein eine einen auf im bei sie wir ihr ihre nicht auch \
         zum zur über aus oder sind unsere unser ihren sich wird werden startseite willkommen \
         unternehmen angebote",
    ),
    (
        "it",
        "il della dei delle degli che sono alla nel nella gli più anche tutti tutte tuo tua \
         nostro nostra nostri sito azienda prodotti migliori notizie benvenuti scopri",
    ),
    (
        "nl",
        "het een voor op zijn bij uw ons onze ook naar uit aan jouw wij welkom bedrijf \
         producten nieuws",
    ),
    (
        "sv",
        "och för att ett ni är inte våra vår välkommen företag",
    ),
    ("pl", "się jest dla od oraz że jak przez są witamy strona"),
    (
        "tr",
        "ve bir ile için bu çok olarak daha veya hakkında anasayfa",
    ),
    (
        "id",
        "yang untuk dengan ini dari pada dalam kami anda atau adalah",
    ),
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

/// The language `text` is in, when its [`COMMON_WORDS`] say so clearly:
/// [`MIN_WORDS`] or more of one language's, and more than twice as many
/// as of any other. `None` for a short text, a name or a mix ("Hotel
/// Casa del Mar | Beachfront rooms"), which say too little.
pub(crate) fn language_by_words(text: &str) -> Option<&'static str> {
    let words = common_words();
    let mut counts = [0; COMMON_WORDS.len()];
    for word in text.split(|c: char| !c.is_alphabetic()) {
        if let Some(&i) = words.get(word.to_lowercase().as_str()) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_word_is_common_to_two_languages() {
        let listed: usize = COMMON_WORDS
            .iter()
            .map(|(_, words)| words.split_whitespace().count())
            .sum();
        assert_eq!(common_words().len(), listed);
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
                "CNN Brasil | Notícias Ao Vivo do Brasil e do Mundo. Pense bem, pense CNN, \
                 notícias ao vivo e fatos da política nacional",
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
        ] {
            assert_eq!(language_by_words(text), None, "{text}");
        }
    }
}

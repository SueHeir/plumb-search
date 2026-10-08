use super::*;
use crate::facts::Fact;

fn article(title: &str, facts: &[(FactKind, &str)]) -> Article {
    Article {
        title: title.into(),
        views: 1,
        facts: facts
            .iter()
            .map(|&(kind, value)| Fact {
                kind,
                value: value.into(),
            })
            .collect(),
        ..Article::default()
    }
}

fn book() -> FactBook {
    use FactKind::*;
    let mut book = FactBook::new();
    for a in [
        article(
            "Albert Einstein",
            &[(Born, "1879-03-14"), (Died, "1955-04-18")],
        ),
        article(
            "Australia",
            &[
                (Capital, "Canberra"),
                (Population, "27204809;2024"),
                (Area, "7688287000000"),
            ],
        ),
        article("Canberra", &[(Population, "466566")]),
        article("Sydney", &[(Population, "5450496")]),
        article("Mount Everest", &[(Elevation, "8848.86")]),
        article(
            "Apple Inc.",
            &[
                (Founded, "1976-04-01"),
                (Founder, "Steve Jobs"),
                (Founder, "Steve Wozniak"),
            ],
        ),
        article("Steve Jobs", &[(Born, "1955-02-24")]),
        article("Steve Wozniak", &[(Born, "1950-08-11")]),
        article("Bill Gates", &[(Born, "1955-10-28")]),
        article("Pac (wrestler)", &[(Born, "1986-08-22")]),
        article("Kate Upton", &[(Height, "1.78"), (Born, "1992-06-10")]),
        article("Inception", &[(Director, "Christopher Nolan")]),
        article("Christopher Nolan", &[(Born, "1970-07-30")]),
        article("Ridley Scott", &[(Born, "1937-11-30")]),
        article("J. K. Rowling", &[(Born, "1965-07-31")]),
        article("France", &[(Population, "68000000")]),
        article("New South Wales", &[]),
    ] {
        book.add(&a);
    }
    book
}

fn checks(text: &str) -> Vec<(String, FactKind, bool)> {
    let book = book();
    book.check(text)
        .into_iter()
        .map(|c| (book.title(c.entity).to_string(), c.kind, c.agrees))
        .collect()
}

fn one(title: &str, kind: FactKind, agrees: bool) -> Vec<(String, FactKind, bool)> {
    vec![(title.to_string(), kind, agrees)]
}

#[test]
fn birth_dates_after_born() {
    use FactKind::*;
    assert_eq!(
        checks("Albert Einstein was born on 14 March 1879 in Ulm."),
        one("Albert Einstein", Born, true)
    );
    assert_eq!(
        checks("Albert Einstein was born in 1879."),
        one("Albert Einstein", Born, true)
    );
    assert_eq!(
        checks("Albert Einstein was born on March 14, 1879."),
        one("Albert Einstein", Born, true)
    );
    assert_eq!(
        checks("Albert Einstein was born in 1882 in Ulm."),
        one("Albert Einstein", Born, false)
    );
    // A year off is not counted.
    assert!(checks("Albert Einstein was born in 1880 in Ulm.").is_empty());
    assert_eq!(
        checks("Albert Einstein was born on 14 May 1879."),
        one("Albert Einstein", Born, false)
    );
    // A day off is not counted either way.
    assert!(checks("Albert Einstein was born on 15 March 1879.").is_empty());
    assert_eq!(checks("Einstein's teacher Albert Einstein's work"), vec![]);
}

#[test]
fn lifespans_in_brackets() {
    use FactKind::*;
    assert_eq!(
        checks("Albert Einstein (14 March 1879 – 18 April 1955) was a physicist."),
        vec![
            ("Albert Einstein".to_string(), Born, true),
            ("Albert Einstein".to_string(), Died, true)
        ]
    );
    assert_eq!(
        checks("Albert Einstein (1879-1962) was a physicist who was born in Germany."),
        vec![
            ("Albert Einstein".to_string(), Born, true),
            ("Albert Einstein".to_string(), Died, false)
        ]
    );
    assert_eq!(
        checks("Kate Upton (born 1992) is a model."),
        one("Kate Upton", Born, true)
    );
    // Approximate dates say nothing, nor do terms of office.
    assert!(checks("Albert Einstein (c. 1879 – 1955) died").is_empty());
    assert!(checks("Albert Einstein (1933-1945) worked").is_empty());
    assert!(checks("Albert Einstein (1919–1920, 1930–1933) worked").is_empty());
    assert!(checks("Albert Einstein (1879-1979) was remembered").is_empty());
    // Another man of the name, or no lifespan.
    assert!(checks("Albert Einstein (1753-1827) farmed").is_empty());
    assert!(checks("Albert Einstein (1930 - 1955) banknote").is_empty());
}

#[test]
fn names_must_be_capitalized_unqualified_and_the_nearest() {
    // A bracketed title is left out; the bare name is not its own.
    assert!(checks("Pac was born in 1990.").is_empty());
    assert!(checks("albert einstein was born in 1880.").is_empty());
    // Another person between the name and "born": not Einstein's birth.
    assert!(
        checks("Albert Einstein's son, with Bill Gates, was born in 1904.")
            .iter()
            .all(|(title, _, _)| title != "Albert Einstein")
    );
    // A possessive is the name.
    assert_eq!(
        checks("Kate Upton's height is 1.78 m."),
        one("Kate Upton", FactKind::Height, true)
    );
}

#[test]
fn decades_and_other_numbers_are_no_dates() {
    assert!(checks("Albert Einstein was born in the 1870s.").is_empty());
    assert!(checks("Albert Einstein was born 3 days before 1880.").is_empty());
    // Another subject between the name and the cue.
    assert!(checks("Albert Einstein's friend Marcel was born in 1876.").is_empty());
    assert!(checks("Mileva (wife of Albert Einstein) was born in 1875.").is_empty());
    assert!(checks("The Albert Einstein company he founded in 2012").is_empty());
    assert!(checks("Albert Einstein (the society) was founded in 1990, born 1990.").is_empty());
}

#[test]
fn populations_areas_and_heights() {
    use FactKind::*;
    assert_eq!(
        checks("Australia has a population of about 27 million."),
        one("Australia", Population, true)
    );
    assert_eq!(
        checks("The population of Australia is 27,204,809 (2024)."),
        one("Australia", Population, true)
    );
    assert_eq!(
        checks("Australia has a population of 12 million people."),
        one("Australia", Population, false)
    );
    // A census a few years old is neither.
    assert!(checks("Australia's population was 20 million in 2000.").is_empty());
    assert!(checks("Australia's population density is 3 per km2.").is_empty());
    assert!(checks("Australia has a population of more than 12 million.").is_empty());
    assert!(checks("The largest city of Australia has a population of 5 million.").is_empty());
    assert!(checks("Sydney is a city in Australia with a population of 5 million.").is_empty());
    assert!(checks("The metropolitan population of Sydney is 9 million.").is_empty());
    assert_eq!(
        checks("Australia has a total area of 7,688,287 km2."),
        one("Australia", Area, true)
    );
    assert_eq!(
        checks("Australia covers an area of 2,968,464 square miles."),
        one("Australia", Area, true)
    );
    assert_eq!(
        checks("Mount Everest has an elevation of 8,849 m."),
        one("Mount Everest", Elevation, true)
    );
    assert_eq!(
        checks("Mount Everest has an elevation of 29,032 feet."),
        one("Mount Everest", Elevation, true)
    );
    assert_eq!(
        checks("Mount Everest has an elevation of 6,000 m."),
        one("Mount Everest", Elevation, false)
    );
    assert_eq!(
        checks("Kate Upton is 5 ft 10 in tall."),
        one("Kate Upton", Height, true)
    );
    assert_eq!(
        checks("Kate Upton stands at 1.60 m."),
        one("Kate Upton", Height, false)
    );
}

#[test]
fn names_as_values() {
    use FactKind::*;
    assert_eq!(
        checks("The capital of Australia is Canberra."),
        one("Australia", Capital, true)
    );
    assert_eq!(
        checks("The capital of Australia is Sydney."),
        one("Australia", Capital, false)
    );
    assert_eq!(
        checks("Sydney is the capital of Australia."),
        one("Australia", Capital, false)
    );
    assert_eq!(
        checks("Canberra is the capital of Australia."),
        one("Australia", Capital, true)
    );
    assert_eq!(
        checks("Australia's capital, Canberra, is inland."),
        one("Australia", Capital, true)
    );
    assert_eq!(
        checks("Inception was directed by Christopher Nolan."),
        one("Inception", Director, true)
    );
    assert_eq!(
        checks("Inception was directed by Ridley Scott."),
        one("Inception", Director, false)
    );
    assert_eq!(
        checks("Apple Inc. was founded by Steve Jobs and Steve Wozniak in 1976."),
        one("Apple Inc.", Founded, true)
    );
    assert_eq!(
        checks("Christopher Nolan, director of Inception, said"),
        one("Inception", Director, true)
    );
    // "capital" in another sense names no capital.
    assert!(checks("Australia has strict capital controls.").is_empty());
    assert!(checks("Officials met in Australia's capital to talk.").is_empty());
    assert!(checks("Australia Capital Region, Sydney").is_empty());
    assert!(checks("Canberra is the capital city of Sydney.").is_empty());
    assert_eq!(
        checks("Australia's capital city of Canberra"),
        one("Australia", Capital, true)
    );
    assert_eq!(
        checks("Canberra, the capital of Australia, is inland."),
        one("Australia", Capital, true)
    );
    // Something else's capital, or a capital of another kind.
    assert!(checks("Sydney, the capital of Australia's New South Wales").is_empty());
    assert!(checks("Sydney, the capital of Australia Capital Territory").is_empty());
    assert!(checks("Sydney, the financial capital of Australia").is_empty());
    assert!(checks("Sydney was the capital of Australia in 1900.").is_empty());
    assert!(checks("Australia's capital is Sydney's rival.").is_empty());
    assert!(
        checks("Near Canberra, the capital of Australia, is Sydney.")
            .iter()
            .all(|c| c.2)
    );
}

#[test]
fn initials_join_like_titles() {
    assert_eq!(
        checks("J. K. Rowling was born on 31 July 1965."),
        one("J. K. Rowling", FactKind::Born, true)
    );
}

#[test]
fn sentences_split_at_full_stops_but_not_titles() {
    assert!(
        checks("Albert Einstein was a physicist. Bill Gates was born in 1955.")
            .iter()
            .all(|(title, _, _)| title != "Albert Einstein")
    );
    assert_eq!(
        checks("Bill Gates was born in 1955."),
        one("Bill Gates", FactKind::Born, true)
    );
}

#[test]
fn shared_names_and_one_word_names() {
    let mut book = FactBook::new();
    book.add(&article("Jordan", &[(FactKind::Population, "11000000")]));
    book.add(&article("Jordan", &[(FactKind::Born, "1963-02-17")]));
    assert_eq!(book.entities(), 1);
    // The second Jordan made the name nobody's for sure.
    assert!(book.check("Jordan was born in 1963.").is_empty());
    assert!(book
        .check("Jordan has a population of 11 million.")
        .is_empty());
}

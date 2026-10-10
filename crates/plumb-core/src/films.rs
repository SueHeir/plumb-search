//! Films and TV shows: the words a page of the `films` set is described
//! with, shared by the fetch that writes them and the search that reads
//! them.
//!
//! A film's description starts with what it is and who made it, then
//! when, then its best-known cast: "Film by Christopher Nolan, 2010 · with
//! Leonardo DiCaprio, Elliot Page". A show's says when it ran: "TV series
//! by Vince Gilligan, 2008–2013 · with Bryan Cranston, Aaron Paul".

/// What a film is, as its description starts.
pub const FILM_KINDS: &[&str] = &["Film", "Animated film", "Documentary film", "TV film"];

/// What a show is, as its description starts.
pub const SHOW_KINDS: &[&str] = &[
    "TV series",
    "Miniseries",
    "Animated series",
    "Anime series",
    "Web series",
];

/// Words that, after a film's title, ask for the film: "inception movie".
pub const FILM_WORDS: &[&str] = &["film", "films", "movie", "movies"];

/// Words that, after a show's title, ask for the show: "breaking bad tv
/// show".
pub const SHOW_WORDS: &[&str] = &[
    "tv",
    "show",
    "shows",
    "series",
    "television",
    "miniseries",
    "anime",
];

/// What is put between a film's byline and its cast.
pub const CAST_SEPARATOR: &str = " · with ";

/// Whether `description`, a page's of the `films` set, is a show's.
pub fn describes_a_show(description: &str) -> bool {
    SHOW_KINDS.iter().any(|kind| {
        description
            .strip_prefix(kind)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', ',']))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_shows_from_films() {
        assert!(describes_a_show("TV series by Vince Gilligan, 2008–2013"));
        assert!(describes_a_show("Miniseries, 2019"));
        assert!(describes_a_show("Anime series"));
        assert!(!describes_a_show("TV film by Mick Jackson, 1984"));
        assert!(!describes_a_show("Film by Christopher Nolan, 2010"));
        assert!(!describes_a_show("TV seriesx"));
    }
}

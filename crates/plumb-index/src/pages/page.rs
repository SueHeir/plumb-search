//! Reading a page from each set's items: what it is called, what it is
//! about and which set and site it belongs to.

use plumb_core::article::{article_url, Article, SearchContent};

use super::{
    Page, BOOKS_SET, DOCS_SET, ENGLISH_WIKIPEDIA, FILMS_SET, GITHUB_SET, MUSIC_SET, PACKAGES_SET,
    PAPERS_SET, PODCASTS_SET, RECORDS_SET, REFERENCE2_SET, REFERENCE_SET, STACKEXCHANGE_SET,
    STACKOVERFLOW_SET, SUBPAGES_SET, WIKIDATA_SET, WIKTIONARY_SET,
};

/// Where a description stops saying what its page is and goes on to who
/// made it or what it is part of.
const DESCRIPTION_HEAD_ENDS: &[&str] = &[
    " by ",
    " directed ",
    " created ",
    " starring ",
    " based ",
    " written ",
    " recorded ",
    " performed ",
    " of ",
    " from ",
    " (",
    ",",
];

impl Page {
    /// The Wikipedia article `article` of Wikipedia in `lang`.
    pub fn from_article(lang: &str, article: Article) -> Self {
        Page {
            paper: None,
            set: format!("wikipedia-{lang}"),
            url: article_url(lang, &article.title),
            title: article.title,
            description: article.description,
            site: article.site,
            views: article.views,
            aliases: article.aliases,
            item: article.item,
            profiles: article.profiles,
            website: article.website,
            package: None,
            facts: article.facts,
            lead: article.lead,
            names: article.names,
            sections: Vec::new(),
            search: None,
            content_language: article
                .language
                .as_deref()
                .and_then(plumb_core::language_code),
        }
    }

    /// The GitHub repository `repo`, written as an article: its title is
    /// `owner/name`, its views its stars, its site its homepage's domain.
    pub fn from_repo(repo: Article) -> Self {
        Page {
            paper: None,
            set: GITHUB_SET.to_string(),
            url: format!("https://github.com/{}", repo.title),
            title: repo.title,
            description: repo.description,
            site: repo.site,
            views: repo.views,
            aliases: repo.aliases,
            item: None,
            profiles: Vec::new(),
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
            sections: Vec::new(),
            search: None,
            content_language: repo.language.as_deref().and_then(plumb_core::language_code),
        }
    }

    /// The Stack Overflow question `question`, written as an article whose
    /// item is the question's id and whose description is its tags.
    pub fn from_question(question: Article) -> Self {
        Page {
            paper: None,
            set: STACKOVERFLOW_SET.to_string(),
            url: format!(
                "https://stackoverflow.com/questions/{}",
                question.item.as_deref().unwrap_or("")
            ),
            title: question.title,
            description: question.description,
            site: None,
            views: question.views,
            aliases: question.aliases,
            item: None,
            profiles: Vec::new(),
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
            sections: Vec::new(),
            search: None,
            content_language: question
                .language
                .as_deref()
                .and_then(plumb_core::language_code),
        }
    }

    /// The question `question` of another Stack Exchange site, written as
    /// an article whose item is the site and question number
    /// (`diy.stackexchange.com/12345`) and whose description is its tags;
    /// `None` for a site not in [`plumb_core::stack_exchange::SITES`].
    pub fn from_exchange(question: Article) -> Option<Self> {
        let (site, id) =
            plumb_core::stack_exchange::parse_question_item(question.item.as_deref()?)?;
        Some(Page {
            paper: None,
            set: STACKEXCHANGE_SET.to_string(),
            url: site.question_url(id),
            title: question.title,
            description: question.description,
            site: None,
            views: question.views,
            aliases: question.aliases,
            item: None,
            profiles: Vec::new(),
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
            sections: Vec::new(),
            search: None,
            content_language: question
                .language
                .as_deref()
                .and_then(plumb_core::language_code),
        })
    }

    /// The Stack Exchange site of a page of the `stackexchange` set.
    fn exchange_site(&self) -> Option<&'static plumb_core::stack_exchange::ExchangeSite> {
        if self.set != STACKEXCHANGE_SET {
            return None;
        }
        let host = self.url.strip_prefix("https://")?.split('/').next()?;
        plumb_core::stack_exchange::site_of(host)
    }

    /// Whether the page is a Wikipedia article.
    pub fn is_article(&self) -> bool {
        self.set.starts_with("wikipedia-")
    }

    /// Whether the page is about a person: it has a birth date.
    pub fn is_person(&self) -> bool {
        self.facts
            .iter()
            .any(|fact| fact.kind == plumb_core::facts::FactKind::Born)
    }

    /// Whether the page is a question, of Stack Overflow or another Stack
    /// Exchange site.
    pub fn is_question(&self) -> bool {
        self.set == STACKOVERFLOW_SET || self.set == STACKEXCHANGE_SET
    }

    /// The Open Library work `book`, written as an article whose item is
    /// the work id (`OL45804W`).
    pub fn from_book(book: Article) -> Self {
        Page {
            paper: None,
            set: BOOKS_SET.to_string(),
            url: format!(
                "https://openlibrary.org/works/{}",
                book.item.as_deref().unwrap_or("")
            ),
            title: book.title,
            description: book.description,
            site: None,
            views: book.views,
            aliases: book.aliases,
            item: None,
            profiles: Vec::new(),
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
            sections: Vec::new(),
            search: None,
            content_language: book.language.as_deref().and_then(plumb_core::language_code),
        }
    }

    /// The podcast `podcast`, written as an article whose item is its
    /// Podcast Index id and whose site is its website's, when it has one
    /// of its own.
    pub fn from_podcast(podcast: Article) -> Self {
        Page {
            paper: None,
            set: PODCASTS_SET.to_string(),
            url: format!(
                "https://podcastindex.org/podcast/{}",
                podcast.item.as_deref().unwrap_or("")
            ),
            title: podcast.title,
            description: podcast.description,
            site: podcast.site,
            views: podcast.views,
            aliases: podcast.aliases,
            item: None,
            profiles: podcast.profiles,
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
            sections: Vec::new(),
            search: None,
            content_language: podcast
                .language
                .as_deref()
                .and_then(plumb_core::language_code),
        }
    }

    /// The song or album `music`, written as an article whose item is
    /// `recording/MBID` or `release-group/MBID` and whose views are its
    /// listeners; `None` for an item that is neither.
    pub fn from_music(music: Article) -> Option<Self> {
        let item = music.item.as_deref()?;
        let (kind, mbid) = item.split_once('/')?;
        let mbid_ok = mbid.len() == 36 && mbid.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
        if !matches!(kind, "recording" | "release-group") || !mbid_ok {
            return None;
        }
        Some(Page {
            paper: None,
            set: MUSIC_SET.to_string(),
            url: format!("https://musicbrainz.org/{item}"),
            title: music.title,
            description: music.description,
            site: None,
            views: music.views,
            aliases: music.aliases,
            item: None,
            profiles: music.profiles,
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
            sections: Vec::new(),
            search: None,
            content_language: music
                .language
                .as_deref()
                .and_then(plumb_core::language_code),
        })
    }

    /// The film or TV show `film`, written as an article whose item is its
    /// Wikidata item (`Q25188`), followed by `/` and its English Wikipedia
    /// article's address path when it has one (`Q25188/Inception`), and
    /// whose views are its sitelinks. Its address is that article, or else
    /// the item on Wikidata. `None` for an item that does not read.
    pub fn from_film(film: Article) -> Option<Self> {
        let item = film.item.as_deref()?;
        let (id, article) = match item.split_once('/') {
            Some((id, path)) => (id, Some(path)),
            None => (item, None),
        };
        if !is_item_id(id) {
            return None;
        }
        let url = match article {
            None => format!("https://www.wikidata.org/wiki/{id}"),
            Some(path)
                if !path.is_empty()
                    && path.chars().all(|c| {
                        c.is_ascii_graphic() && !matches!(c, '"' | '<' | '>' | '\\' | '|')
                    }) =>
            {
                format!("{ENGLISH_WIKIPEDIA}{path}")
            }
            Some(_) => return None,
        };
        Some(Page {
            paper: None,
            set: FILMS_SET.to_string(),
            url,
            title: film.title,
            description: film.description,
            site: None,
            views: film.views,
            aliases: film.aliases,
            item: Some(id.to_string()),
            profiles: film.profiles,
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
            sections: Vec::new(),
            search: None,
            content_language: film.language.as_deref().and_then(plumb_core::language_code),
        })
    }

    /// The software docs page `doc`, written as an article whose item is
    /// the page's address (see `plumb_ingest::docs`). `None` for an item
    /// that is not an https:// address.
    pub fn from_docs(doc: Article) -> Option<Self> {
        let url = doc.item.filter(|item| {
            item.starts_with("https://")
                && item.len() > "https://".len()
                && !item.contains(char::is_whitespace)
        })?;
        Some(Page {
            paper: None,
            set: DOCS_SET.to_string(),
            url,
            title: doc.title,
            description: doc.description,
            site: None,
            views: doc.views,
            aliases: doc.aliases,
            item: None,
            profiles: Vec::new(),
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
            sections: doc.sections,
            search: doc.search.as_ref().and_then(SearchContent::bounded),
            content_language: doc.language.as_deref().and_then(plumb_core::language_code),
        })
    }

    /// The page of the reference set written as `page` in its articles
    /// file, whose item is its address.
    pub fn from_reference(page: Article) -> Option<Self> {
        Page::from_site_page(REFERENCE_SET, page)
    }

    /// The page of the set `set` (reference or subpages) written as `page`
    /// in its articles file, whose item is its address.
    fn from_site_page(set: &str, page: Article) -> Option<Self> {
        let url = page.item.filter(|item| {
            item.starts_with("https://")
                && item.len() > "https://".len()
                && !item.contains(char::is_whitespace)
        })?;
        Some(Page {
            paper: None,
            set: set.to_string(),
            url,
            title: page.title,
            description: page.description,
            site: None,
            views: page.views,
            aliases: page.aliases,
            item: None,
            profiles: Vec::new(),
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
            sections: Vec::new(),
            search: page.search.as_ref().and_then(SearchContent::bounded),
            content_language: page.language.as_deref().and_then(plumb_core::language_code),
        })
    }

    /// Whether the page is an inner page of a site of the reference or
    /// subpages set, found by the same rules.
    pub fn is_site_page(&self) -> bool {
        is_site_page_set(&self.set)
    }

    /// The host of a docs, reference, subpages or record page's address,
    /// without `www.`: `docs.python.org`, `healthline.com`.
    fn docs_host(&self) -> Option<&str> {
        if self.set != DOCS_SET && self.set != RECORDS_SET && !self.is_site_page() {
            return None;
        }
        let host = self
            .url
            .split_once("://")?
            .1
            .split(['/', '?', '#'])
            .next()
            .filter(|host| !host.is_empty())?;
        Some(host.strip_prefix("www.").unwrap_or(host))
    }

    /// Where a paper can be read free, when it is not its own address:
    /// its copy on arXiv, the publisher's open version, a repository's.
    pub fn free_copy(&self) -> Option<&str> {
        if self.set != PAPERS_SET {
            return None;
        }
        self.website.as_deref().filter(|url| *url != self.url)
    }

    /// Whether the page is a TV show of the films set, rather than a film.
    pub fn is_show(&self) -> bool {
        self.set == FILMS_SET
            && self
                .description
                .as_deref()
                .is_some_and(plumb_core::films::describes_a_show)
    }

    /// Whether the page is a film or show of the films set that English
    /// Wikipedia has an article on, which is its address.
    pub fn is_film_with_article(&self) -> bool {
        self.set == FILMS_SET && self.url.starts_with(ENGLISH_WIKIPEDIA)
    }

    /// Whether the page is a song of the music set, rather than an album.
    pub fn is_song(&self) -> bool {
        self.set == MUSIC_SET && self.url.starts_with("https://musicbrainz.org/recording/")
    }

    /// Whether the page is a work of one of `kinds`, as its title's
    /// qualifier ("Barbie (film)", "Hades (video game)") or the head of its
    /// description ("1986 action drama film by Tony Scott", "Song by the
    /// Beatles, 1968") calls it.
    pub fn is_work_of(&self, kinds: &[&str]) -> bool {
        let qualifier = self
            .title
            .strip_suffix(')')
            .and_then(|title| title.rsplit_once(" ("));
        let description = self.description.as_deref().unwrap_or("").to_lowercase();
        let head_end = DESCRIPTION_HEAD_ENDS
            .iter()
            .filter_map(|end| description.find(end))
            .min()
            .unwrap_or(description.len());
        qualifier.is_some_and(|(_, what)| {
            what.split_whitespace()
                .any(|word| kinds.contains(&word.to_lowercase().as_str()))
        }) || description[..head_end]
            .split_whitespace()
            .last()
            .is_some_and(|word| kinds.contains(&word))
    }

    /// The paper `paper`, written as an article whose item is its DOI or
    /// else its OpenAlex id, whose views are its citations and whose
    /// website is where it can be read free.
    pub fn from_paper(paper: Article) -> Self {
        let item = paper.item.as_deref().unwrap_or("");
        let url = if item.starts_with("10.") {
            format!("https://doi.org/{item}")
        } else {
            format!("https://openalex.org/{item}")
        };
        Page {
            paper: paper.paper,
            set: PAPERS_SET.to_string(),
            url,
            title: paper.title,
            description: paper.description,
            site: None,
            views: paper.views,
            aliases: paper.aliases,
            item: None,
            profiles: Vec::new(),
            website: paper.website,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
            sections: Vec::new(),
            search: paper.search,
            content_language: paper
                .language
                .as_deref()
                .and_then(plumb_core::language_code),
        }
    }

    /// The Wikidata item `item`, written as an article whose title is its
    /// English name and whose views are its sitelinks.
    pub fn from_item(item: Article) -> Self {
        Page {
            paper: None,
            set: WIKIDATA_SET.to_string(),
            url: format!(
                "https://www.wikidata.org/wiki/{}",
                item.item.as_deref().unwrap_or("")
            ),
            title: item.title,
            description: item.description,
            site: item.site,
            views: item.views,
            aliases: item.aliases,
            item: item.item,
            profiles: item.profiles,
            website: None,
            package: None,
            facts: item.facts,
            lead: None,
            names: Vec::new(),
            sections: Vec::new(),
            search: None,
            content_language: item.language.as_deref().and_then(plumb_core::language_code),
        }
    }

    /// The English word `word` of Wiktionary, written as an article whose
    /// title is the word and whose description says what it means.
    pub fn from_word(word: Article) -> Self {
        let url = plumb_core::article::article_url("en", &word.title).replacen(
            "en.wikipedia.org",
            "en.wiktionary.org",
            1,
        );
        Page {
            paper: None,
            set: WIKTIONARY_SET.to_string(),
            url,
            title: word.title,
            description: word.description,
            site: None,
            views: word.views,
            aliases: Vec::new(),
            item: None,
            profiles: Vec::new(),
            website: None,
            package: None,
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
            sections: Vec::new(),
            search: None,
            content_language: word.language.as_deref().and_then(plumb_core::language_code),
        }
    }

    /// The software package `package`, written as an article whose item is
    /// `registry:name` and whose views are its share of its registry's
    /// most used package's downloads (see [`plumb_core::packages`]).
    /// `None` when it has no card, or one whose registry or name does not
    /// read.
    pub fn from_package(package: Article) -> Option<Self> {
        let info = package.package?;
        let url = info.page_url()?;
        Some(Page {
            paper: None,
            set: PACKAGES_SET.to_string(),
            url,
            title: package.title,
            description: package.description,
            site: None,
            views: package.views,
            aliases: package.aliases,
            item: None,
            profiles: Vec::new(),
            website: None,
            package: Some(info),
            facts: Vec::new(),
            lead: None,
            names: Vec::new(),
            sections: Vec::new(),
            search: None,
            content_language: package
                .language
                .as_deref()
                .and_then(plumb_core::language_code),
        })
    }

    /// Whether [`Page::from_set`] can read pages of the set `set`. A
    /// package's line needs its card, so an empty article can't tell.
    pub fn has_reader(set: &str) -> bool {
        set == PACKAGES_SET
            || set == STACKEXCHANGE_SET
            || set == MUSIC_SET
            || set == FILMS_SET
            || set == DOCS_SET
            || set == REFERENCE_SET
            || set == REFERENCE2_SET
            || set == SUBPAGES_SET
            || Page::from_set(set, Article::default()).is_some()
    }

    /// The page of the set `set` written as `article` in its articles
    /// file, `None` for a set without a reader (or a package without a card,
    /// or a question of a site not known).
    pub fn from_set(set: &str, article: Article) -> Option<Self> {
        Some(match set {
            GITHUB_SET => Page::from_repo(article),
            STACKOVERFLOW_SET => Page::from_question(article),
            BOOKS_SET => Page::from_book(article),
            PODCASTS_SET => Page::from_podcast(article),
            PAPERS_SET => Page::from_paper(article),
            WIKIDATA_SET => Page::from_item(article),
            WIKTIONARY_SET => Page::from_word(article),
            PACKAGES_SET => Page::from_package(article)?,
            STACKEXCHANGE_SET => Page::from_exchange(article)?,
            MUSIC_SET => Page::from_music(article)?,
            FILMS_SET => Page::from_film(article)?,
            DOCS_SET => Page::from_docs(article)?,
            REFERENCE_SET => Page::from_reference(article)?,
            REFERENCE2_SET => Page::from_site_page(REFERENCE2_SET, article)?,
            SUBPAGES_SET => Page::from_site_page(SUBPAGES_SET, article)?,
            _ => Page::from_article(set.strip_prefix("wikipedia-")?, article),
        })
    }

    /// The words a question, paper, docs or reference page is found by
    /// besides its title: its title, a question's tags and the titles of
    /// its duplicates, and a docs, reference or subpages page's other names and
    /// description. `None` for pages of other sets, found by their names
    /// only.
    pub fn topic(&self) -> Option<String> {
        if self.set == PAPERS_SET {
            let mut topic = self.title.clone();
            for text in self
                .aliases
                .iter()
                .chain(&self.description)
                .chain(self.paper.iter().flat_map(|p| p.authors.iter()))
            {
                topic.push(' ');
                topic.push_str(text);
            }
            return Some(topic);
        }
        if self.set == DOCS_SET || self.is_site_page() {
            let mut topic = self.title.clone();
            for text in self
                .aliases
                .iter()
                .chain(&self.description)
                .chain(&self.sections)
            {
                topic.push(' ');
                topic.push_str(text);
            }
            if let Some(search) = &self.search {
                topic.push(' ');
                topic.push_str(&search.text());
            }
            return Some(topic);
        }
        self.is_question().then(|| {
            let mut topic = self.title.clone();
            for text in self.aliases.iter().chain(&self.description) {
                topic.push(' ');
                topic.push_str(text);
            }
            topic
        })
    }

    /// The titles a question is asked by, each with its tags: its own,
    /// then those of the questions closed as its duplicates. A paper or
    /// docs page is asked by its title and [`Page::topic`], pages of other
    /// sets by none.
    pub(crate) fn asked_as(&self) -> Vec<(&str, String)> {
        if !self.is_question() {
            return self
                .topic()
                .map(|topic| (self.title.as_str(), topic))
                .into_iter()
                .collect();
        }
        std::iter::once(&self.title)
            .chain(&self.aliases)
            .map(|title| {
                let words = match &self.description {
                    Some(tags) => format!("{title} {tags}"),
                    None => title.clone(),
                };
                (title.as_str(), words)
            })
            .collect()
    }

    /// What a Wikipedia article is matched on beyond its names: its title,
    /// the other titles that lead to it and its description ("1973 studio
    /// album by Queen"), so "queen album" finds Queen (album). `None` for
    /// pages of other sets.
    pub fn about(&self) -> Option<String> {
        if !self.is_article() {
            return None;
        }
        let mut about = self.title.clone();
        for text in self
            .aliases
            .iter()
            .chain(&self.names)
            .chain(&self.description)
        {
            about.push(' ');
            about.push_str(text);
        }
        Some(about)
    }

    /// Whether the page may be listed before every site. Books, podcasts,
    /// papers, songs, albums, films and shows share their titles with too
    /// much ("Python", "Apple", "Hello", "Up") to.
    pub fn may_lead(&self) -> bool {
        self.set != BOOKS_SET
            && self.set != PAPERS_SET
            && self.set != PODCASTS_SET
            && self.set != MUSIC_SET
            && self.set != FILMS_SET
    }

    /// The name of the set people see: "Wikipedia".
    pub fn set_name(&self) -> &str {
        if self.set.starts_with("wikipedia-") || self.is_film_with_article() {
            "Wikipedia"
        } else if self.set == GITHUB_SET {
            "GitHub"
        } else if self.set == STACKOVERFLOW_SET {
            "Stack Overflow"
        } else if let Some(site) = self.exchange_site() {
            site.name
        } else if self.set == BOOKS_SET {
            "Open Library"
        } else if self.set == PODCASTS_SET {
            "Podcast Index"
        } else if self.set == MUSIC_SET {
            "MusicBrainz"
        } else if self.set == PAPERS_SET {
            "OpenAlex"
        } else if self.set == WIKIDATA_SET || self.set == FILMS_SET {
            "Wikidata"
        } else if self.set == WIKTIONARY_SET {
            "Wiktionary"
        } else if let Some(registry) = self.registry() {
            registry.name
        } else if let Some(host) = self.docs_host() {
            host
        } else {
            &self.set
        }
    }

    /// The registry of a package.
    pub fn registry(&self) -> Option<&'static plumb_core::packages::Registry> {
        self.package.as_ref()?.registry()
    }

    /// The language the page is in, when its set says: `en` for
    /// English Wikipedia, GitHub and Stack Exchange's questions.
    pub fn language(&self) -> Option<&str> {
        if let Some(lang) = self
            .content_language
            .as_deref()
            .filter(|lang| plumb_core::language_code(lang).as_deref() == Some(*lang))
        {
            Some(lang)
        } else if let Some(lang) = self.set.strip_prefix("wikipedia-") {
            Some(lang)
        } else if self.set == GITHUB_SET || self.is_question() || self.is_film_with_article() {
            Some("en")
        } else {
            None
        }
    }

    /// The site whose icon marks the page: wikipedia.org, github.com.
    pub fn set_domain(&self) -> &str {
        if self.set == GITHUB_SET {
            "github.com"
        } else if self.set == STACKOVERFLOW_SET {
            "stackoverflow.com"
        } else if let Some(site) = self.exchange_site() {
            site.domain
        } else if self.set == BOOKS_SET {
            "openlibrary.org"
        } else if self.set == PODCASTS_SET {
            "podcastindex.org"
        } else if self.set == MUSIC_SET {
            "musicbrainz.org"
        } else if self.set == PAPERS_SET {
            "openalex.org"
        } else if self.set == FILMS_SET && !self.is_film_with_article() {
            "wikidata.org"
        } else if let Some(registry) = self.registry() {
            registry.domain
        } else if let Some(host) = self.docs_host() {
            host
        } else {
            "wikipedia.org"
        }
    }
}

/// Whether `set` holds inner pages of sites, of the reference or
/// subpages sets ([`Page::is_site_page`]).
pub(crate) fn is_site_page_set(set: &str) -> bool {
    set == REFERENCE_SET || set == REFERENCE2_SET || set == SUBPAGES_SET
}

/// Whether `id` is a Wikidata item's: `Q25188`.
pub(crate) fn is_item_id(id: &str) -> bool {
    id.len() > 1 && id.starts_with('Q') && id[1..].bytes().all(|b| b.is_ascii_digit())
}

//! Software documentation: the docs sites whose pages make the `docs` page
//! set, and how a docs page's title is read.
//!
//! Each site's pages are those its sitemaps (or its table of contents)
//! list under its roots, fetched like homepages: robots.txt obeyed, one
//! request at a time. A page is named by its title without the site's own
//! name ("Sorting Techniques — Python 3.14 documentation" is "Sorting
//! Techniques") and found by that title after the product's name ("python
//! sorting techniques") or by most of its words.

/// A docs site whose pages are listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocsSite {
    /// Short name to pick it by: `python`.
    pub key: &'static str,
    /// What the docs are of, as people name it in a search: "Python".
    pub product: &'static str,
    /// Registrable domain of the site, as Plumb counts them (docs.python.org
    /// is a site of its own).
    pub domain: &'static str,
    /// Where its docs pages are; only pages under these are taken.
    pub roots: &'static [&'static str],
    /// Its sitemaps, when robots.txt names none or names others.
    pub sitemaps: &'static [&'static str],
    /// Pages linking to all its docs pages (a table of contents).
    pub index_pages: &'static [&'static str],
    /// Other names its page titles end in, dropped like the product's:
    /// "MDN" for "Array.prototype.sort() - JavaScript | MDN".
    pub names: &'static [&'static str],
    /// How much its pages weigh against other sites', 1 to 10: the most
    /// used docs weigh most.
    pub weight: u64,
}

/// The docs sites, the most used first.
pub const DOCS_SITES: &[DocsSite] = &[
    DocsSite {
        key: "mdn",
        product: "MDN",
        domain: "developer.mozilla.org",
        roots: &["https://developer.mozilla.org/en-US/docs/"],
        sitemaps: &["https://developer.mozilla.org/sitemaps/en-us/sitemap.xml.gz"],
        index_pages: &[],
        names: &["MDN Web Docs", "MDN"],
        weight: 10,
    },
    DocsSite {
        key: "python",
        product: "Python",
        domain: "docs.python.org",
        roots: &["https://docs.python.org/3/"],
        sitemaps: &[],
        index_pages: &[
            "https://docs.python.org/3/contents.html",
            "https://docs.python.org/3/py-modindex.html",
        ],
        names: &[],
        weight: 10,
    },
    DocsSite {
        key: "rust",
        product: "Rust",
        domain: "doc.rust-lang.org",
        roots: &[
            "https://doc.rust-lang.org/std/",
            "https://doc.rust-lang.org/book/",
            "https://doc.rust-lang.org/reference/",
            "https://doc.rust-lang.org/cargo/",
            "https://doc.rust-lang.org/rust-by-example/",
        ],
        sitemaps: &[],
        index_pages: &[
            "https://doc.rust-lang.org/std/all.html",
            "https://doc.rust-lang.org/book/print.html",
            "https://doc.rust-lang.org/reference/print.html",
            "https://doc.rust-lang.org/cargo/print.html",
        ],
        names: &[
            "The Rust Programming Language",
            "The Rust Reference",
            "The Cargo Book",
        ],
        weight: 8,
    },
    DocsSite {
        key: "go",
        product: "Go",
        domain: "go.dev",
        roots: &["https://go.dev/doc/", "https://go.dev/ref/"],
        sitemaps: &[],
        index_pages: &["https://go.dev/doc/"],
        names: &["The Go Programming Language"],
        weight: 7,
    },
    DocsSite {
        key: "node",
        product: "Node.js",
        domain: "nodejs.org",
        roots: &["https://nodejs.org/api/", "https://nodejs.org/en/learn/"],
        sitemaps: &[],
        index_pages: &["https://nodejs.org/api/"],
        names: &["Node.js Documentation", "Node.js v"],
        weight: 8,
    },
    DocsSite {
        key: "typescript",
        product: "TypeScript",
        domain: "typescriptlang.org",
        roots: &["https://www.typescriptlang.org/docs/"],
        sitemaps: &[],
        index_pages: &["https://www.typescriptlang.org/docs/"],
        names: &[],
        weight: 7,
    },
    DocsSite {
        key: "react",
        product: "React",
        domain: "react.dev",
        roots: &["https://react.dev/reference/", "https://react.dev/learn"],
        sitemaps: &[],
        index_pages: &["https://react.dev/reference/react"],
        names: &[],
        weight: 8,
    },
    DocsSite {
        key: "vue",
        product: "Vue",
        domain: "vuejs.org",
        roots: &["https://vuejs.org/guide/", "https://vuejs.org/api/"],
        sitemaps: &[],
        index_pages: &["https://vuejs.org/api/"],
        names: &["Vue.js"],
        weight: 6,
    },
    DocsSite {
        key: "nextjs",
        product: "Next.js",
        domain: "nextjs.org",
        roots: &["https://nextjs.org/docs/"],
        sitemaps: &[],
        index_pages: &[],
        names: &[],
        weight: 6,
    },
    DocsSite {
        key: "tailwind",
        product: "Tailwind CSS",
        domain: "tailwindcss.com",
        roots: &["https://tailwindcss.com/docs/"],
        sitemaps: &[],
        index_pages: &["https://tailwindcss.com/docs/installation"],
        names: &["TailwindCSS"],
        weight: 6,
    },
    DocsSite {
        key: "django",
        product: "Django",
        domain: "djangoproject.com",
        roots: &["https://docs.djangoproject.com/en/stable/"],
        sitemaps: &[],
        index_pages: &["https://docs.djangoproject.com/en/stable/contents/"],
        names: &["Django documentation"],
        weight: 7,
    },
    DocsSite {
        key: "flask",
        product: "Flask",
        domain: "palletsprojects.com",
        roots: &["https://flask.palletsprojects.com/en/stable/"],
        sitemaps: &[],
        index_pages: &["https://flask.palletsprojects.com/en/stable/"],
        names: &["Flask Documentation"],
        weight: 5,
    },
    DocsSite {
        key: "numpy",
        product: "NumPy",
        domain: "numpy.org",
        roots: &["https://numpy.org/doc/stable/"],
        sitemaps: &[],
        index_pages: &[
            "https://numpy.org/doc/stable/reference/index.html",
            "https://numpy.org/doc/stable/user/index.html",
        ],
        names: &["NumPy Manual", "NumPy v"],
        weight: 6,
    },
    DocsSite {
        key: "pandas",
        product: "pandas",
        domain: "pydata.org",
        roots: &["https://pandas.pydata.org/docs/"],
        sitemaps: &[],
        index_pages: &[
            "https://pandas.pydata.org/docs/reference/index.html",
            "https://pandas.pydata.org/docs/user_guide/index.html",
        ],
        names: &["pandas documentation"],
        weight: 6,
    },
    DocsSite {
        key: "pytorch",
        product: "PyTorch",
        domain: "pytorch.org",
        roots: &["https://docs.pytorch.org/docs/stable/"],
        sitemaps: &[],
        index_pages: &["https://docs.pytorch.org/docs/stable/index.html"],
        names: &["PyTorch documentation", "PyTorch"],
        weight: 6,
    },
    DocsSite {
        key: "postgres",
        product: "PostgreSQL",
        domain: "postgresql.org",
        roots: &["https://www.postgresql.org/docs/current/"],
        sitemaps: &[],
        index_pages: &["https://www.postgresql.org/docs/current/index.html"],
        names: &["PostgreSQL Documentation"],
        weight: 7,
    },
    DocsSite {
        key: "mysql",
        product: "MySQL",
        domain: "mysql.com",
        roots: &["https://dev.mysql.com/doc/refman/8.4/en/"],
        sitemaps: &[],
        index_pages: &["https://dev.mysql.com/doc/refman/8.4/en/"],
        names: &["MySQL 8.4 Reference Manual", "MySQL"],
        weight: 6,
    },
    DocsSite {
        key: "sqlite",
        product: "SQLite",
        domain: "sqlite.org",
        roots: &["https://www.sqlite.org/"],
        sitemaps: &[],
        index_pages: &[
            "https://www.sqlite.org/docs.html",
            "https://www.sqlite.org/lang.html",
        ],
        names: &[],
        weight: 5,
    },
    DocsSite {
        key: "docker",
        product: "Docker",
        domain: "docker.com",
        roots: &["https://docs.docker.com/"],
        sitemaps: &[],
        index_pages: &[],
        names: &["Docker Docs"],
        weight: 7,
    },
    DocsSite {
        key: "kubernetes",
        product: "Kubernetes",
        domain: "kubernetes.io",
        roots: &["https://kubernetes.io/docs/"],
        sitemaps: &[],
        index_pages: &[],
        names: &[],
        weight: 7,
    },
    DocsSite {
        key: "git",
        product: "Git",
        domain: "git-scm.com",
        roots: &[
            "https://git-scm.com/docs/",
            "https://git-scm.com/book/en/v2/",
        ],
        sitemaps: &[],
        index_pages: &["https://git-scm.com/docs", "https://git-scm.com/book/en/v2"],
        names: &["Git Documentation"],
        weight: 7,
    },
    DocsSite {
        key: "github",
        product: "GitHub",
        domain: "docs.github.com",
        roots: &["https://docs.github.com/en/"],
        sitemaps: &[],
        index_pages: &[],
        names: &["GitHub Docs"],
        weight: 7,
    },
    DocsSite {
        key: "java",
        product: "Java",
        domain: "oracle.com",
        roots: &["https://docs.oracle.com/en/java/javase/21/docs/api/"],
        sitemaps: &[],
        index_pages: &["https://docs.oracle.com/en/java/javase/21/docs/api/allclasses-index.html"],
        names: &["Java SE 21 & JDK 21"],
        weight: 6,
    },
    DocsSite {
        key: "cpp",
        product: "C++",
        domain: "cppreference.com",
        roots: &["https://en.cppreference.com/w/"],
        sitemaps: &[],
        index_pages: &[
            "https://en.cppreference.com/w/cpp/symbol_index",
            "https://en.cppreference.com/w/c",
        ],
        names: &["cppreference.com"],
        weight: 6,
    },
    DocsSite {
        key: "dotnet",
        product: ".NET",
        domain: "learn.microsoft.com",
        roots: &[
            "https://learn.microsoft.com/en-us/dotnet/",
            "https://learn.microsoft.com/en-us/powershell/",
        ],
        sitemaps: &[],
        index_pages: &[],
        names: &["Microsoft Learn"],
        weight: 6,
    },
    DocsSite {
        key: "kotlin",
        product: "Kotlin",
        domain: "kotlinlang.org",
        roots: &[
            "https://kotlinlang.org/docs/",
            "https://kotlinlang.org/api/",
        ],
        sitemaps: &[],
        index_pages: &[],
        names: &["Kotlin Documentation"],
        weight: 5,
    },
    DocsSite {
        key: "php",
        product: "PHP",
        domain: "php.net",
        roots: &["https://www.php.net/manual/en/"],
        sitemaps: &[],
        index_pages: &["https://www.php.net/manual/en/indexes.functions.php"],
        names: &["Manual"],
        weight: 6,
    },
    DocsSite {
        key: "ruby",
        product: "Ruby",
        domain: "ruby-lang.org",
        roots: &["https://docs.ruby-lang.org/en/master/"],
        sitemaps: &[],
        index_pages: &["https://docs.ruby-lang.org/en/master/index.html"],
        names: &["Documentation for Ruby"],
        weight: 5,
    },
    DocsSite {
        key: "rails",
        product: "Rails",
        domain: "rubyonrails.org",
        roots: &["https://guides.rubyonrails.org/"],
        sitemaps: &[],
        index_pages: &["https://guides.rubyonrails.org/"],
        names: &["Ruby on Rails Guides"],
        weight: 5,
    },
    DocsSite {
        key: "man",
        product: "Linux",
        domain: "man7.org",
        roots: &["https://man7.org/linux/man-pages/"],
        sitemaps: &[],
        index_pages: &["https://man7.org/linux/man-pages/dir_all_alphabetic.html"],
        names: &["Linux manual page"],
        weight: 6,
    },
    DocsSite {
        key: "bash",
        product: "Bash",
        domain: "gnu.org",
        roots: &["https://www.gnu.org/software/bash/manual/html_node/"],
        sitemaps: &[],
        index_pages: &["https://www.gnu.org/software/bash/manual/html_node/index.html"],
        names: &["Bash Reference Manual"],
        weight: 5,
    },
    DocsSite {
        key: "archwiki",
        product: "Arch Linux",
        domain: "archlinux.org",
        roots: &["https://wiki.archlinux.org/title/"],
        sitemaps: &[],
        index_pages: &["https://wiki.archlinux.org/title/Table_of_contents"],
        names: &["ArchWiki"],
        weight: 6,
    },
    DocsSite {
        key: "nginx",
        product: "nginx",
        domain: "nginx.org",
        roots: &["https://nginx.org/en/docs/"],
        sitemaps: &[],
        index_pages: &[
            "https://nginx.org/en/docs/dirindex.html",
            "https://nginx.org/en/docs/",
        ],
        names: &[],
        weight: 5,
    },
    DocsSite {
        key: "redis",
        product: "Redis",
        domain: "redis.io",
        roots: &["https://redis.io/docs/"],
        sitemaps: &[],
        index_pages: &[],
        names: &["Docs"],
        weight: 5,
    },
    DocsSite {
        key: "mongodb",
        product: "MongoDB",
        domain: "mongodb.com",
        roots: &["https://www.mongodb.com/docs/manual/"],
        sitemaps: &[],
        index_pages: &[],
        names: &["Database Manual", "MongoDB Docs"],
        weight: 5,
    },
    DocsSite {
        key: "terraform",
        product: "Terraform",
        domain: "hashicorp.com",
        roots: &["https://developer.hashicorp.com/terraform/"],
        sitemaps: &[],
        index_pages: &[],
        names: &["HashiCorp Developer"],
        weight: 5,
    },
    DocsSite {
        key: "godot",
        product: "Godot",
        domain: "godotengine.org",
        roots: &["https://docs.godotengine.org/en/stable/"],
        sitemaps: &[],
        index_pages: &["https://docs.godotengine.org/en/stable/index.html"],
        names: &["Godot Engine documentation", "Godot Engine"],
        weight: 4,
    },
    DocsSite {
        key: "bootstrap",
        product: "Bootstrap",
        domain: "getbootstrap.com",
        roots: &["https://getbootstrap.com/docs/5.3/"],
        sitemaps: &[],
        index_pages: &["https://getbootstrap.com/docs/5.3/getting-started/introduction/"],
        names: &["Bootstrap v5.3"],
        weight: 4,
    },
];

/// Words that end a docs page's title as the site's name, not the page's:
/// "Python 3.14 documentation".
const SITE_WORDS: &[&str] = &["documentation", "docs", "manual", "reference manual"];

/// What separates a page's title from the site's name in its `<title>`.
const SEPARATORS: &[&str] = &[" — ", " – ", " | ", " - ", " · ", " :: ", " » "];

/// The page's own title out of `title`, the `<title>` of a page of `site`:
/// the parts before those that name the site, joined by " — ".
/// "Sorting Techniques — Python 3.14 documentation" is "Sorting
/// Techniques"; "Array.prototype.sort() - JavaScript | MDN" is
/// "Array.prototype.sort() — JavaScript". `None` when nothing is left but
/// the site's name.
pub fn page_title(site: &DocsSite, title: &str) -> Option<String> {
    let title = crate::collapse_whitespace(title);
    let mut parts: Vec<&str> = vec![title.as_str()];
    for separator in SEPARATORS {
        parts = parts
            .into_iter()
            .flat_map(|part| part.split(separator))
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect();
    }
    let names_site = |part: &str| {
        let lower = part.to_lowercase();
        let is_name = |name: &str| {
            let name = name.to_lowercase();
            lower == name || lower.starts_with(&format!("{name} "))
        };
        is_name(site.product)
            || site.names.iter().any(|name| is_name(name))
            || SITE_WORDS
                .iter()
                .any(|word| lower == *word || lower.ends_with(&format!(" {word}")))
    };
    while parts.len() > 1 && parts.last().is_some_and(|part| names_site(part)) {
        parts.pop();
    }
    if parts.len() == 1 && names_site(parts[0]) {
        return None;
    }
    Some(parts.join(" — "))
}

/// The site of `key`.
pub fn site(key: &str) -> Option<&'static DocsSite> {
    DOCS_SITES.iter().find(|site| site.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_unique_and_roots_are_https() {
        for (i, site) in DOCS_SITES.iter().enumerate() {
            assert!(
                DOCS_SITES[..i].iter().all(|other| other.key != site.key),
                "{} twice",
                site.key
            );
            assert!((1..=10).contains(&site.weight), "{}", site.key);
            for root in site
                .roots
                .iter()
                .chain(site.index_pages)
                .chain(site.sitemaps)
            {
                assert!(root.starts_with("https://"), "{root}");
            }
            for root in site.roots {
                assert_eq!(
                    crate::registrable_domain(root).as_deref(),
                    Some(site.domain),
                    "{root}"
                );
            }
        }
    }

    #[test]
    fn titles_lose_the_site_name() {
        let python = site("python").unwrap();
        assert_eq!(
            page_title(python, "Sorting Techniques — Python 3.14.0 documentation").as_deref(),
            Some("Sorting Techniques")
        );
        assert_eq!(
            page_title(
                python,
                "os.path — Common pathname manipulations — Python 3.14.0 documentation"
            )
            .as_deref(),
            Some("os.path — Common pathname manipulations")
        );
        assert_eq!(page_title(python, "3.14.0 Documentation"), None);
        assert_eq!(page_title(python, "Python 3.14 documentation"), None);
        let mdn = site("mdn").unwrap();
        assert_eq!(
            page_title(mdn, "Array.prototype.sort() - JavaScript | MDN").as_deref(),
            Some("Array.prototype.sort() — JavaScript")
        );
        assert_eq!(
            page_title(mdn, "gap - CSS: Cascading Style Sheets | MDN").as_deref(),
            Some("gap — CSS: Cascading Style Sheets")
        );
        let man = site("man").unwrap();
        assert_eq!(
            page_title(man, "ls(1) - Linux manual page").as_deref(),
            Some("ls(1)")
        );
        let rust = site("rust").unwrap();
        assert_eq!(
            page_title(rust, "Vec in std::vec - Rust").as_deref(),
            Some("Vec in std::vec")
        );
        assert_eq!(
            page_title(rust, "Built-in   Functions").as_deref(),
            Some("Built-in Functions")
        );
    }
}

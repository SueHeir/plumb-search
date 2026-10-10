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
    /// Words that, in a search, ask about what the docs are of (lowercase):
    /// a docs page is only found by most of a search's words when one of
    /// these is among them ("python", "golang").
    pub asked_by: &'static [&'static str],
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
        asked_by: &["mdn", "javascript", "js", "css", "html", "dom", "http"],
        domain: "developer.mozilla.org",
        roots: &[
            "https://developer.mozilla.org/en-US/docs/",
            "https://developer.mozilla.org/es/docs/",
            "https://developer.mozilla.org/de/docs/",
        ],
        sitemaps: &["https://developer.mozilla.org/sitemaps/en-us/sitemap.xml.gz"],
        index_pages: &[
            "https://developer.mozilla.org/es/docs/Web/JavaScript",
            "https://developer.mozilla.org/de/docs/Web/JavaScript",
        ],
        names: &["MDN Web Docs", "MDN"],
        weight: 10,
    },
    DocsSite {
        key: "python",
        product: "Python",
        asked_by: &["python", "python3", "py"],
        domain: "docs.python.org",
        roots: &[
            "https://docs.python.org/3/",
            "https://docs.python.org/es/3/",
        ],
        sitemaps: &[],
        index_pages: &[
            "https://docs.python.org/3/contents.html",
            "https://docs.python.org/3/py-modindex.html",
            // The contents leave out the HOWTOs ("Sorting Techniques").
            "https://docs.python.org/3/howto/index.html",
            "https://docs.python.org/es/3/contents.html",
            "https://docs.python.org/es/3/py-modindex.html",
            "https://docs.python.org/es/3/howto/index.html",
        ],
        names: &[],
        weight: 10,
    },
    DocsSite {
        key: "rust",
        product: "Rust",
        asked_by: &["rust", "rustlang", "cargo"],
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
            // The books' chapters, which their one-page prints link to
            // only by anchors.
            "https://doc.rust-lang.org/book/toc.html",
            "https://doc.rust-lang.org/reference/toc.html",
            "https://doc.rust-lang.org/cargo/toc.html",
            "https://doc.rust-lang.org/rust-by-example/toc.html",
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
        asked_by: &["go", "golang"],
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
        asked_by: &["node", "node.js", "nodejs"],
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
        asked_by: &["typescript", "ts"],
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
        asked_by: &["react", "reactjs", "react.js"],
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
        asked_by: &["vue", "vuejs", "vue.js"],
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
        asked_by: &["next.js", "nextjs"],
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
        asked_by: &["tailwind", "tailwindcss"],
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
        asked_by: &["django"],
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
        asked_by: &["flask"],
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
        asked_by: &["numpy", "np"],
        domain: "numpy.org",
        roots: &["https://numpy.org/doc/stable/"],
        sitemaps: &[],
        index_pages: &[
            "https://numpy.org/doc/stable/reference/index.html",
            "https://numpy.org/doc/stable/user/index.html",
            // Each function's page ("numpy.reshape"), from the lists of
            // routines.
            "https://numpy.org/doc/stable/reference/routines.array-creation.html",
            "https://numpy.org/doc/stable/reference/routines.array-manipulation.html",
            "https://numpy.org/doc/stable/reference/routines.math.html",
            "https://numpy.org/doc/stable/reference/routines.linalg.html",
            "https://numpy.org/doc/stable/reference/routines.sort.html",
            "https://numpy.org/doc/stable/reference/routines.statistics.html",
            "https://numpy.org/doc/stable/reference/routines.logic.html",
            "https://numpy.org/doc/stable/reference/random/index.html",
        ],
        names: &["NumPy Manual", "NumPy v"],
        weight: 6,
    },
    DocsSite {
        key: "pandas",
        product: "pandas",
        asked_by: &["pandas", "dataframe", "pd"],
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
        asked_by: &["pytorch", "torch"],
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
        asked_by: &["postgres", "postgresql", "psql"],
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
        asked_by: &["mysql"],
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
        asked_by: &["sqlite", "sqlite3"],
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
        asked_by: &["docker", "dockerfile"],
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
        asked_by: &["kubernetes", "k8s", "kubectl"],
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
        asked_by: &["git"],
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
        asked_by: &["github"],
        domain: "docs.github.com",
        roots: &["https://docs.github.com/en/"],
        sitemaps: &[],
        // No sitemap: each product's landing page lists its articles.
        index_pages: &[
            "https://docs.github.com/en",
            "https://docs.github.com/en/get-started",
            "https://docs.github.com/en/actions",
            "https://docs.github.com/en/repositories",
            "https://docs.github.com/en/pull-requests",
            "https://docs.github.com/en/issues",
            "https://docs.github.com/en/authentication",
            "https://docs.github.com/en/pages",
            "https://docs.github.com/en/packages",
            "https://docs.github.com/en/codespaces",
            "https://docs.github.com/en/copilot",
            "https://docs.github.com/en/rest",
            "https://docs.github.com/en/code-security",
            "https://docs.github.com/en/organizations",
            "https://docs.github.com/en/account-and-profile",
            "https://docs.github.com/en/github-cli",
        ],
        names: &["GitHub Docs"],
        weight: 7,
    },
    DocsSite {
        key: "java",
        product: "Java",
        asked_by: &["java"],
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
        asked_by: &["c++", "cpp", "std::"],
        domain: "cppreference.com",
        roots: &[
            "https://en.cppreference.com/cpp/",
            "https://en.cppreference.com/c/",
            "https://en.cppreference.com/w/",
        ],
        sitemaps: &[],
        index_pages: &[
            "https://en.cppreference.com/cpp/symbol_index",
            "https://en.cppreference.com/cpp",
            "https://en.cppreference.com/c",
        ],
        names: &["cppreference.com"],
        weight: 6,
    },
    DocsSite {
        key: "dotnet",
        product: ".NET",
        asked_by: &[".net", "dotnet", "c#", "csharp", "powershell"],
        domain: "learn.microsoft.com",
        roots: &[
            "https://learn.microsoft.com/en-us/dotnet/",
            "https://learn.microsoft.com/en-us/powershell/",
        ],
        sitemaps: &[],
        index_pages: &[
            "https://learn.microsoft.com/en-us/dotnet/",
            "https://learn.microsoft.com/en-us/dotnet/csharp/",
            "https://learn.microsoft.com/en-us/dotnet/fundamentals/",
            "https://learn.microsoft.com/en-us/dotnet/core/introduction",
            "https://learn.microsoft.com/en-us/aspnet/core/",
            "https://learn.microsoft.com/en-us/powershell/scripting/overview",
        ],
        names: &["Microsoft Learn"],
        weight: 6,
    },
    DocsSite {
        key: "kotlin",
        product: "Kotlin",
        asked_by: &["kotlin"],
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
        asked_by: &["php"],
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
        asked_by: &["ruby"],
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
        asked_by: &["rails", "ror"],
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
        asked_by: &["linux", "man", "unix"],
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
        asked_by: &["bash", "shell"],
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
        asked_by: &["arch", "archlinux", "linux"],
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
        asked_by: &["nginx"],
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
        asked_by: &["redis"],
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
        asked_by: &["mongodb", "mongo"],
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
        asked_by: &["terraform", "hcl"],
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
        asked_by: &["godot", "gdscript"],
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
        asked_by: &["bootstrap"],
        domain: "getbootstrap.com",
        roots: &["https://getbootstrap.com/docs/5.3/"],
        sitemaps: &[],
        index_pages: &["https://getbootstrap.com/docs/5.3/getting-started/introduction/"],
        names: &["Bootstrap v5.3"],
        weight: 4,
    },
    DocsSite {
        key: "pytest",
        product: "pytest",
        asked_by: &["pytest"],
        domain: "pytest.org",
        roots: &["https://docs.pytest.org/en/stable/"],
        sitemaps: &[],
        index_pages: &[
            "https://docs.pytest.org/en/stable/how-to/index.html",
            "https://docs.pytest.org/en/stable/reference/index.html",
        ],
        names: &[],
        weight: 5,
    },
    DocsSite {
        key: "jenkins",
        product: "Jenkins",
        asked_by: &["jenkins"],
        domain: "jenkins.io",
        roots: &["https://www.jenkins.io/doc/"],
        sitemaps: &[],
        index_pages: &["https://www.jenkins.io/doc/book/"],
        names: &[],
        weight: 5,
    },
    DocsSite {
        key: "prometheus",
        product: "Prometheus",
        asked_by: &["prometheus", "promql"],
        domain: "prometheus.io",
        roots: &["https://prometheus.io/docs/"],
        sitemaps: &[],
        index_pages: &[
            "https://prometheus.io/docs/introduction/overview/",
            "https://prometheus.io/docs/prometheus/latest/querying/basics/",
        ],
        names: &[],
        weight: 6,
    },
    DocsSite {
        key: "typescript-eslint",
        product: "typescript-eslint",
        asked_by: &["typescript-eslint"],
        domain: "typescript-eslint.io",
        roots: &[
            "https://typescript-eslint.io/getting-started/",
            "https://typescript-eslint.io/rules/",
            "https://typescript-eslint.io/users/",
            "https://typescript-eslint.io/packages/",
        ],
        sitemaps: &[],
        index_pages: &[
            "https://typescript-eslint.io/getting-started/",
            "https://typescript-eslint.io/rules/",
        ],
        names: &[],
        weight: 5,
    },
    DocsSite {
        key: "vite",
        product: "Vite",
        asked_by: &["vite", "vitejs"],
        domain: "vite.dev",
        roots: &["https://vite.dev/guide/", "https://vite.dev/config/"],
        sitemaps: &[],
        index_pages: &["https://vite.dev/guide/", "https://vite.dev/config/"],
        names: &[],
        weight: 6,
    },
    DocsSite {
        key: "gitlab",
        product: "GitLab",
        asked_by: &["gitlab"],
        domain: "gitlab.com",
        roots: &["https://docs.gitlab.com/"],
        sitemaps: &[],
        index_pages: &[
            "https://docs.gitlab.com/ci/",
            "https://docs.gitlab.com/user/",
        ],
        names: &[],
        weight: 6,
    },
    DocsSite {
        key: "grafana",
        product: "Grafana",
        asked_by: &["grafana"],
        domain: "grafana.com",
        roots: &["https://grafana.com/docs/grafana/latest/"],
        sitemaps: &[],
        index_pages: &[
            "https://grafana.com/docs/grafana/latest/introduction/",
            "https://grafana.com/docs/grafana/latest/fundamentals/getting-started/",
        ],
        names: &[],
        weight: 6,
    },
];

/// Words that end a docs page's title as the site's name, not the page's:
/// "Python 3.14 documentation".
const SITE_WORDS: &[&str] = &["documentation", "docs", "manual", "reference manual"];

/// What separates a page's title from the site's name in its `<title>`.
const SEPARATORS: &[&str] = &[" — ", " – ", " | ", " - ", " · ", " :: ", " » "];

/// The page's own title out of `title`, the `<title>` of a page of `site`:
/// the parts between those that name the site, joined by " — ".
/// "Sorting Techniques — Python 3.14 documentation" is "Sorting
/// Techniques"; "Array.prototype.sort() - JavaScript | MDN" is
/// "Array.prototype.sort() — JavaScript"; "TypeScript: Documentation -
/// Generics" is "Generics"; Rust's "Vec in std::vec" is "Vec — std::vec".
/// `None` when nothing is left but the site's name.
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
    // "Vec in std::vec": the item, then the module it is in.
    parts = parts
        .into_iter()
        .flat_map(|part| match part.split_once(" in ") {
            Some((item, path))
                if path.contains("::") && !item.contains(' ') && !path.contains(' ') =>
            {
                vec![item, path]
            }
            _ => vec![part],
        })
        .collect();
    // A leading part that is only the site's name: "TypeScript:
    // Documentation".
    let only_site = |part: &str| {
        let lower = part.trim_end_matches(':').to_lowercase();
        lower == site.product.to_lowercase()
            || site.names.iter().any(|name| lower == name.to_lowercase())
            || SITE_WORDS
                .iter()
                .any(|word| lower.ends_with(&format!(" {word}")))
    };
    while parts.len() > 1 && only_site(parts[0]) {
        parts.remove(0);
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
        // "git-rebase Documentation" is the page's own name.
        let last = parts.last().copied().unwrap_or_default();
        if let Some(own) = own_docs(site, last) {
            let at = parts.len() - 1;
            parts[at] = own;
            break;
        }
        parts.pop();
    }
    if parts.len() == 1 && names_site(parts[0]) {
        return own_docs(site, parts[0]).map(str::to_string);
    }
    Some(parts.join(" — "))
}

/// The page's own name in `part`, a part of a title of `site` ending in
/// a word for docs: "git-rebase" in "git-rebase Documentation". `None`
/// when what comes before that word names the site, perhaps with a
/// version ("Python 3.14 documentation", "3.14.0 Documentation").
fn own_docs<'a>(site: &DocsSite, part: &'a str) -> Option<&'a str> {
    let lower = part.to_lowercase();
    let word = SITE_WORDS
        .iter()
        .find(|word| lower.ends_with(&format!(" {word}")))?;
    let own = part[..part.len() - word.len() - 1].trim();
    let rest: Vec<String> = own
        .split_whitespace()
        .filter(|w| !w.chars().any(|c| c.is_ascii_digit()))
        .map(str::to_lowercase)
        .collect();
    let rest = rest.join(" ");
    let names_site = rest.is_empty()
        || rest == "the"
        || [site.product].iter().chain(site.names).any(|name| {
            let name = name.to_lowercase();
            rest == name
                || rest.starts_with(&format!("{name} "))
                || rest.starts_with(&format!("the {name}"))
        });
    (!names_site).then_some(own)
}

/// Words that ask for docs in general, not for something in them: "python
/// docs" asks for the docs site, not one of its pages.
const DOCS_WORDS: &[&str] = &[
    "doc",
    "docs",
    "documentation",
    "manual",
    "reference",
    "official",
    "web",
    "site",
    "website",
    "a",
    "an",
    "the",
    "in",
    "of",
    "on",
    "to",
    "for",
    "and",
    "how",
    "what",
    "is",
];

/// The docs site a page at `url` is on: the one with a root it is under.
/// Else the one with a root on its host, as a page its root sent on to
/// another version ("docs.pytorch.org/docs/2.9/" for "…/docs/stable/").
pub fn site_of_url(url: &str) -> Option<&'static DocsSite> {
    DOCS_SITES
        .iter()
        .find(|site| {
            site.roots
                .iter()
                .any(|root| url.starts_with(root.trim_end_matches('/')))
        })
        .or_else(|| {
            let host = crate::host_of(url)?;
            DOCS_SITES.iter().find(|site| {
                site.roots
                    .iter()
                    .any(|root| crate::host_of(root).as_deref() == Some(host.as_str()))
            })
        })
}

/// Whether `query` asks about something in `site`'s docs: it names what
/// the docs are of ([`DocsSite::asked_by`]) and something more than docs
/// in general. "python sort list" does; "python docs", "mdn web docs" and
/// "note taking app" do not.
pub fn asks_about(site: &DocsSite, query: &str) -> bool {
    let words: Vec<String> = query
        .split_whitespace()
        .map(|word| {
            let word = word.to_lowercase();
            let word = word
                .strip_suffix("'s")
                .or_else(|| word.strip_suffix("’s"))
                .unwrap_or(&word);
            word.trim_matches(|c: char| !c.is_alphanumeric() && !"+#".contains(c))
                .to_string()
        })
        .filter(|word| !word.is_empty())
        .collect();
    let names = |word: &str| {
        site.asked_by
            .iter()
            .any(|asked| word == *asked || (asked.ends_with("::") && word.starts_with(asked)))
    };
    words.iter().any(|word| names(word))
        && words
            .iter()
            .any(|word| !names(word) && !DOCS_WORDS.contains(&word.as_str()))
}

/// The docs site that `name` (lowercase, as a search writes it) names:
/// its key, what its docs are of, another of its names or a word that asks
/// about it. "postgres" and "mdn" name a docs site; "github" does not.
pub fn named_site(name: &str) -> Option<&'static DocsSite> {
    let name = name.trim().to_lowercase();
    DOCS_SITES.iter().find(|site| {
        site.key == name
            || site.product.to_lowercase() == name
            || site.names.iter().any(|other| other.to_lowercase() == name)
            || site.asked_by.contains(&name.as_str())
    })
}

/// The site of `key`.
pub fn site(key: &str) -> Option<&'static DocsSite> {
    DOCS_SITES.iter().find(|site| site.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_docs_sites_by_what_they_document() {
        let named = |name: &str| named_site(name).map(|site| site.key);
        assert_eq!(named("postgres"), Some("postgres"));
        assert_eq!(named("MDN"), Some("mdn"));
        assert_eq!(named("python"), Some("python"));
        assert_eq!(named("golang"), Some("go"));
        assert_eq!(named("note taking"), None);
    }

    #[test]
    fn keys_are_unique_and_roots_are_https() {
        for (i, site) in DOCS_SITES.iter().enumerate() {
            assert!(
                DOCS_SITES[..i].iter().all(|other| other.key != site.key),
                "{} twice",
                site.key
            );
            assert!((1..=10).contains(&site.weight), "{}", site.key);
            assert!(!site.asked_by.is_empty(), "{}", site.key);
            for word in site.asked_by {
                assert_eq!(*word, word.to_lowercase(), "{}", site.key);
            }
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
    fn searches_ask_about_docs_when_they_name_the_product() {
        let python = site("python").unwrap();
        assert!(asks_about(python, "sort a list in python"));
        assert!(asks_about(python, "Python's f-string format"));
        assert!(!asks_about(python, "python docs"));
        assert!(!asks_about(python, "python"));
        assert!(!asks_about(python, "module object is not callable"));
        let mdn = site("mdn").unwrap();
        assert!(!asks_about(mdn, "mdn web docs"));
        assert!(!asks_about(mdn, "note taking app"));
        assert!(asks_about(mdn, "css grid layout"));
        let cpp = site("cpp").unwrap();
        assert!(asks_about(cpp, "std::vector push_back"));
        assert!(asks_about(cpp, "c++ vector"));
        assert_eq!(
            site_of_url("https://docs.python.org/3/howto/sorting.html").map(|s| s.key),
            Some("python")
        );
        assert_eq!(
            site_of_url("https://react.dev/learn/thinking-in-react").map(|s| s.key),
            Some("react")
        );
        assert!(site_of_url("https://example.com/").is_none());
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
            Some("Vec — std::vec")
        );
        assert_eq!(
            page_title(
                rust,
                "Using Trait Objects in Rust - The Rust Programming Language"
            )
            .as_deref(),
            Some("Using Trait Objects in Rust")
        );
        let git = site("git").unwrap();
        assert_eq!(
            page_title(git, "Git - git-rebase Documentation").as_deref(),
            Some("git-rebase")
        );
        assert_eq!(
            site_of_url("https://docs.pytorch.org/docs/2.9/generated/torch.nn.Linear.html")
                .map(|site| site.key),
            Some("pytorch")
        );
        assert_eq!(site_of_url("https://example.com/docs/"), None);
        let typescript = site("typescript").unwrap();
        assert_eq!(
            page_title(typescript, "TypeScript: Documentation - Generics").as_deref(),
            Some("Generics")
        );
        assert_eq!(
            page_title(rust, "Built-in   Functions").as_deref(),
            Some("Built-in Functions")
        );
    }
}

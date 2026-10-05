//! Software packages: the libraries on npm, PyPI, crates.io and the other
//! registries, so a coding agent asking for "serde crate" or "latest
//! version of requests python" gets the package's card (its latest
//! version, the command that installs it, where its docs and code are) in
//! one search result instead of opening and reading three pages.
//!
//! A package travels in its set's articles file ([`crate::article`]) as an
//! article whose title is the package's name and whose item is
//! `registry:name` (`npm:react`), followed by a line of what the card
//! says:
//!
//! ```text
//! package  item  key=value|key=value     (tab-separated)
//! package  npm:react  version=19.2.0|released=2026-09-30|license=MIT|docs=https://react.dev/|repo=https://github.com/facebook/react
//! ```
//!
//! Addresses of a package's page on its registry and the command that
//! installs it are made here from the registry and the name, and only for
//! names that look right, so a strange name never makes a strange link or
//! command.

use serde::{Deserialize, Serialize};

/// A package registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Registry {
    /// Short name kept in files and set ids: `npm`.
    pub key: &'static str,
    /// What people see: "npm", "PyPI".
    pub name: &'static str,
    /// The registry's site, whose icon marks its packages: `npmjs.com`.
    pub domain: &'static str,
    /// The registry's name on ecosyste.ms, where the packages are listed
    /// from: `npmjs.org`.
    pub ecosystems: &'static str,
    /// Words in a query that ask for this registry's packages: "npm",
    /// "crate", "pip".
    pub words: &'static [&'static str],
    /// Names of the registry's language, which ask for its packages less
    /// surely: "rust book" is not the crate `book`.
    pub languages: &'static [&'static str],
    /// The package's page, with `{}` for the name.
    page: &'static str,
    /// The command that installs the package, with `{}` for the name.
    install: &'static str,
    /// Where the docs are when the package names none, with `{}` for the
    /// name: docs.rs builds every crate's.
    docs: Option<&'static str>,
    /// Characters a name may have besides ASCII letters and digits.
    name_chars: &'static str,
}

/// The registries there are, the ones agents ask for most first.
pub const REGISTRIES: &[Registry] = &[
    Registry {
        key: "npm",
        name: "npm",
        domain: "npmjs.com",
        ecosystems: "npmjs.org",
        words: &["npm"],
        languages: &["node", "nodejs", "javascript", "typescript"],
        page: "https://www.npmjs.com/package/{}",
        install: "npm install {}",
        docs: None,
        name_chars: "-_.@/",
    },
    Registry {
        key: "pypi",
        name: "PyPI",
        domain: "pypi.org",
        ecosystems: "pypi.org",
        words: &["pypi", "pip"],
        languages: &["python"],
        page: "https://pypi.org/project/{}/",
        install: "pip install {}",
        docs: None,
        name_chars: "-_.",
    },
    Registry {
        key: "crates",
        name: "crates.io",
        domain: "crates.io",
        ecosystems: "crates.io",
        words: &["crate", "crates", "cargo"],
        languages: &["rust"],
        page: "https://crates.io/crates/{}",
        install: "cargo add {}",
        docs: Some("https://docs.rs/{}"),
        name_chars: "-_",
    },
    Registry {
        key: "go",
        name: "Go",
        domain: "pkg.go.dev",
        ecosystems: "proxy.golang.org",
        words: &[],
        languages: &["golang", "go"],
        page: "https://pkg.go.dev/{}",
        install: "go get {}",
        docs: Some("https://pkg.go.dev/{}"),
        name_chars: "-_.~/",
    },
    Registry {
        key: "gem",
        name: "RubyGems",
        domain: "rubygems.org",
        ecosystems: "rubygems.org",
        words: &["gem", "rubygems"],
        languages: &["ruby"],
        page: "https://rubygems.org/gems/{}",
        install: "gem install {}",
        docs: Some("https://www.rubydoc.info/gems/{}"),
        name_chars: "-_.",
    },
    Registry {
        key: "composer",
        name: "Packagist",
        domain: "packagist.org",
        ecosystems: "packagist.org",
        words: &["composer", "packagist"],
        languages: &["php"],
        page: "https://packagist.org/packages/{}",
        install: "composer require {}",
        docs: None,
        name_chars: "-_./",
    },
    Registry {
        key: "nuget",
        name: "NuGet",
        domain: "nuget.org",
        ecosystems: "nuget.org",
        words: &["nuget"],
        languages: &["dotnet", ".net", "c#", "csharp"],
        page: "https://www.nuget.org/packages/{}",
        install: "dotnet add package {}",
        docs: None,
        name_chars: "-_.",
    },
    Registry {
        key: "maven",
        name: "Maven Central",
        domain: "central.sonatype.com",
        ecosystems: "repo1.maven.org",
        words: &["maven", "gradle"],
        languages: &["java", "kotlin"],
        page: "https://central.sonatype.com/artifact/{}",
        install: "",
        docs: Some("https://javadoc.io/doc/{}"),
        name_chars: "-_.:",
    },
];

/// Words in a query that ask for a package of any registry: "serde
/// version", "lodash package".
pub const PACKAGE_WORDS: &[&str] = &["package", "packages", "version", "versions", "changelog"];

/// Words left out of a query asking for a package, so "latest version of
/// tokio" asks for tokio.
pub const FILLER_WORDS: &[&str] = &[
    "latest",
    "current",
    "newest",
    "new",
    "stable",
    "what",
    "whats",
    "what's",
    "is",
    "the",
    "of",
    "for",
    "a",
    "in",
    "docs",
    "documentation",
    "api",
    "reference",
    "release",
    "releases",
    "notes",
    "install",
    "library",
    "lib",
    "module",
    "license",
    "licence",
    "homepage",
    "repo",
    "repository",
    "source",
    "github",
    "download",
    "downloads",
];

/// The registry kept as `key`.
pub fn registry(key: &str) -> Option<&'static Registry> {
    REGISTRIES.iter().find(|r| r.key == key)
}

/// The registry ecosyste.ms calls `name`.
pub fn registry_on_ecosystems(name: &str) -> Option<&'static Registry> {
    REGISTRIES.iter().find(|r| r.ecosystems == name)
}

impl Registry {
    /// Whether `name` looks like a name of this registry's packages.
    pub fn accepts(&self, name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 214
            && !name.starts_with(['.', '/', '-'])
            && !name.contains("..")
            && !name.contains("//")
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || self.name_chars.contains(c))
    }

    /// The package's page on the registry.
    pub fn page_url(&self, name: &str) -> Option<String> {
        if !self.accepts(name) {
            return None;
        }
        // Maven's names are `group:artifact`, its pages group/artifact.
        Some(self.page.replace("{}", &name.replace(':', "/")))
    }

    /// The command that installs the package, `None` for a registry
    /// without one (Maven's depend on the build tool).
    pub fn install(&self, name: &str) -> Option<String> {
        (!self.install.is_empty() && self.accepts(name)).then(|| self.install.replace("{}", name))
    }

    /// Where the package's docs are when it names none itself.
    pub fn default_docs(&self, name: &str) -> Option<String> {
        let docs = self.docs?;
        self.accepts(name)
            .then(|| docs.replace("{}", &name.replace(':', "/")))
    }

    /// The name people say for a package of this registry: "gin" for
    /// `github.com/gin-gonic/gin`, "slf4j-api" for `org.slf4j:slf4j-api`,
    /// "laravel/framework" stays as it is, as people search it so. `None`
    /// when it is the name itself.
    pub fn short_name<'a>(&self, name: &'a str) -> Option<&'a str> {
        let short = match self.key {
            "go" => name.rsplit('/').next(),
            "maven" => name.rsplit(':').next(),
            _ => None,
        }?;
        (!short.is_empty() && short != name).then_some(short)
    }
}

/// The `registry:name` item of a package.
pub fn package_item(registry: &str, name: &str) -> String {
    format!("{registry}:{name}")
}

/// The registry and name of a package's item, when it names a registry
/// there is and a name that looks right.
pub fn parse_package_item(item: &str) -> Option<(&'static Registry, &str)> {
    let (key, name) = item.split_once(':')?;
    let registry = registry(key)?;
    registry.accepts(name).then_some((registry, name))
}

/// What a package's card says besides its name and description.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageInfo {
    /// The registry's key: `npm`.
    pub registry: String,
    /// The package's name on it, as given to the install command.
    pub name: String,
    /// The latest version: `19.2.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// When it came out, `YYYY-MM-DD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released: Option<String>,
    /// Its license, an SPDX expression where the registry gives one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// Where its docs are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs: Option<String>,
    /// Where its code is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Its homepage, when that is neither the docs nor the code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
}

/// What starts a package's line in an articles file.
pub const PACKAGE_LINE: &str = "package\t";

/// Most characters kept of a version or license.
const MAX_SHORT_FIELD: usize = 64;

impl PackageInfo {
    /// The registry the package is on.
    pub fn registry(&self) -> Option<&'static Registry> {
        registry(&self.registry)
    }

    /// The package's page on its registry.
    pub fn page_url(&self) -> Option<String> {
        self.registry()?.page_url(&self.name)
    }

    /// The command that installs it.
    pub fn install(&self) -> Option<String> {
        self.registry()?.install(&self.name)
    }

    /// Where its docs are: its own, else its registry's for it.
    pub fn docs(&self) -> Option<String> {
        self.docs
            .clone()
            .or_else(|| self.registry()?.default_docs(&self.name))
    }

    /// The card in one short line, what an agent needs most first:
    /// "Latest 19.2.0 (2026-09-30), MIT. Install: npm install react. Docs:
    /// https://react.dev/ Code: https://github.com/facebook/react"
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut latest = String::new();
        if let Some(version) = &self.version {
            latest = format!("Latest {version}");
            if let Some(released) = &self.released {
                latest.push_str(&format!(" ({released})"));
            }
        }
        if let Some(license) = &self.license {
            if !latest.is_empty() {
                latest.push_str(", ");
            }
            latest.push_str(license);
        }
        if !latest.is_empty() {
            parts.push(format!("{latest}."));
        }
        if let Some(install) = self.install() {
            parts.push(format!("Install: {install}."));
        }
        if let Some(docs) = self.docs() {
            parts.push(format!("Docs: {docs}"));
        }
        if let Some(repo) = &self.repo {
            parts.push(format!("Code: {repo}"));
        }
        if let Some(home) = &self.homepage {
            parts.push(format!("Home: {home}"));
        }
        parts.join(" ")
    }

    /// The text after the item on the package's line: `version=…|…`.
    pub fn write(&self) -> String {
        let short = |v: &Option<String>| {
            v.as_deref()
                .map(|v| crate::truncate_chars(&clean(v), MAX_SHORT_FIELD))
                .filter(|v| !v.is_empty())
        };
        let url = |v: &Option<String>| v.as_deref().filter(|u| is_link(u)).map(str::to_string);
        let released = self
            .released
            .as_deref()
            .filter(|d| is_date(d))
            .map(str::to_string);
        [
            ("version", short(&self.version)),
            ("released", released),
            ("license", short(&self.license)),
            ("docs", url(&self.docs)),
            ("repo", url(&self.repo)),
            ("home", url(&self.homepage)),
        ]
        .into_iter()
        .filter_map(|(key, value)| Some(format!("{key}={}", value?)))
        .collect::<Vec<_>>()
        .join("|")
    }

    /// Reads a package's line, `None` for another line or a package whose
    /// item does not read. Keys this build does not know are left out.
    pub fn parse_line(line: &str) -> Option<(String, PackageInfo)> {
        let rest = line
            .trim_end_matches(['\n', '\r'])
            .strip_prefix(PACKAGE_LINE)?;
        let (item, fields) = rest.split_once('\t').unwrap_or((rest, ""));
        let item = item.trim();
        let (registry, name) = parse_package_item(item)?;
        let mut info = PackageInfo {
            registry: registry.key.to_string(),
            name: name.to_string(),
            ..PackageInfo::default()
        };
        for pair in fields.split('|') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            let link = || is_link(value).then(|| value.to_string());
            match key.trim() {
                "version" => info.version = Some(value.to_string()),
                "released" if is_date(value) => info.released = Some(value.to_string()),
                "license" => info.license = Some(value.to_string()),
                "docs" => info.docs = link(),
                "repo" => info.repo = link(),
                "home" => info.homepage = link(),
                _ => {}
            }
        }
        Some((item.to_string(), info))
    }
}

/// `text` without characters that would break a field.
fn clean(text: &str) -> String {
    crate::collapse_whitespace(&text.replace(['\t', '\n', '\r', '|', '='], " "))
}

/// Whether `url` is an http(s) address that fits a field.
fn is_link(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://"))
        && url.len() <= 300
        && !url.contains(['\t', '\n', '\r', '|', ' ', '"', '<', '>'])
}

/// Whether `date` is `YYYY-MM-DD`.
fn is_date(date: &str) -> bool {
    let b = date.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

/// What a query asks for when it asks for a package: the name it says,
/// and the registries it names (none for any). `None` when it does not
/// ask for a package: no registry or package word ("serde crate", "lodash
/// version"), or nothing left once those and filler words are taken out.
pub fn package_query(query: &str) -> Option<PackageQuery> {
    let mut registries: Vec<&'static Registry> = Vec::new();
    let mut asked = false;
    let mut surely = false;
    let mut rest: Vec<&str> = Vec::new();
    for word in query.split_whitespace() {
        let lower = word.to_lowercase();
        let lower = lower.trim_matches(|c: char| matches!(c, ',' | '?' | '!' | '"' | '\''));
        let explicit = REGISTRIES.iter().any(|r| r.words.contains(&lower));
        let named: Vec<_> = REGISTRIES
            .iter()
            .filter(|r| r.words.contains(&lower) || r.languages.contains(&lower))
            .collect();
        if !named.is_empty() {
            surely |= explicit;
            for registry in named {
                if !registries.contains(&registry) {
                    registries.push(registry);
                }
            }
            asked = true;
        } else if PACKAGE_WORDS.contains(&lower) {
            asked = true;
            surely |= lower.starts_with("package");
        } else if !FILLER_WORDS.contains(&lower) {
            rest.push(word);
        }
    }
    if !asked || rest.is_empty() {
        return None;
    }
    Some(PackageQuery {
        name: rest.join(" "),
        registries,
        surely,
    })
}

/// See [`package_query`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageQuery {
    /// What is left of the query: the package's name, as typed.
    pub name: String,
    /// The registries the query names; empty for any.
    pub registries: Vec<&'static Registry>,
    /// The query names a registry ("crate", "npm") or says "package",
    /// rather than only a language ("rust") or "version": any package of
    /// the name will do,
    /// not only a well-known one.
    pub surely: bool,
}

impl PackageQuery {
    /// Whether a package of `registry` is one asked for.
    pub fn wants(&self, registry: &str) -> bool {
        self.registries.is_empty() || self.registries.iter().any(|r| r.key == registry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_round_trip() {
        let info = PackageInfo {
            registry: "npm".into(),
            name: "@types/node".into(),
            version: Some("24.5.2".into()),
            released: Some("2026-09-30".into()),
            license: Some("MIT".into()),
            docs: None,
            repo: Some("https://github.com/DefinitelyTyped/DefinitelyTyped".into()),
            homepage: Some("bad link".into()),
        };
        let line = format!("{PACKAGE_LINE}npm:@types/node\t{}", info.write());
        assert_eq!(
            line,
            "package\tnpm:@types/node\tversion=24.5.2|released=2026-09-30|license=MIT|\
             repo=https://github.com/DefinitelyTyped/DefinitelyTyped"
        );
        let (item, read) = PackageInfo::parse_line(&line).unwrap();
        assert_eq!(item, "npm:@types/node");
        assert_eq!(
            read,
            PackageInfo {
                homepage: None,
                ..info
            }
        );
        assert_eq!(
            read.page_url().as_deref(),
            Some("https://www.npmjs.com/package/@types/node")
        );
        assert_eq!(read.install().as_deref(), Some("npm install @types/node"));
        assert_eq!(
            read.summary(),
            "Latest 24.5.2 (2026-09-30), MIT. Install: npm install @types/node. \
             Code: https://github.com/DefinitelyTyped/DefinitelyTyped"
        );
        // Unknown keys and registries are left out.
        let (_, read) =
            PackageInfo::parse_line("package\tcrates:serde\tversion=1.0.228|stars=9").unwrap();
        assert_eq!(read.docs().as_deref(), Some("https://docs.rs/serde"));
        assert!(PackageInfo::parse_line("package\tcpan:Moose\tversion=2").is_none());
        assert!(PackageInfo::parse_line("profiles\tQ1\tx=y").is_none());
    }

    #[test]
    fn strange_names_make_no_links() {
        let npm = registry("npm").unwrap();
        assert!(npm.page_url("left-pad").is_some());
        for name in ["", "../etc", "a b", "x\"y", "a//b", "-rf"] {
            assert!(npm.page_url(name).is_none(), "{name}");
            assert!(npm.install(name).is_none(), "{name}");
        }
        let maven = registry("maven").unwrap();
        assert_eq!(
            maven.page_url("org.slf4j:slf4j-api").as_deref(),
            Some("https://central.sonatype.com/artifact/org.slf4j/slf4j-api")
        );
        assert_eq!(maven.install("org.slf4j:slf4j-api"), None);
        assert_eq!(maven.short_name("org.slf4j:slf4j-api"), Some("slf4j-api"));
        let go = registry("go").unwrap();
        assert_eq!(go.short_name("github.com/gin-gonic/gin"), Some("gin"));
        assert_eq!(registry("npm").unwrap().short_name("react"), None);
    }

    #[test]
    fn queries_asking_for_packages() {
        let q = package_query("serde crate").unwrap();
        assert_eq!(q.name, "serde");
        assert!(q.wants("crates") && !q.wants("npm") && q.surely);
        let q = package_query("latest version of requests python").unwrap();
        assert_eq!(q.name, "requests");
        assert!(q.wants("pypi") && !q.surely);
        let q = package_query("lodash version").unwrap();
        assert_eq!(q.name, "lodash");
        assert!(q.wants("npm") && q.wants("crates") && !q.surely);
        assert!(package_query("lodash package").unwrap().surely);
        assert_eq!(
            package_query("express npm license").unwrap().name,
            "express"
        );
        assert_eq!(
            package_query("@tanstack/react-query npm").unwrap().name,
            "@tanstack/react-query"
        );
        for query in ["react", "react docs", "python", "latest version", "crate"] {
            assert_eq!(package_query(query), None, "{query}");
        }
    }
}

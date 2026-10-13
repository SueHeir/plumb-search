/* What a Plumb node answers, and the Zotero items made from it.
 *
 * Nothing here touches Zotero, so the tests run this file in Node. Titles,
 * descriptions and addresses come from the open web: the search window
 * only ever sets them as text, and only http and https addresses are kept.
 */

var Plumb = (() => {
  const DEFAULT_NODE = "https://plumbsearch.org";

  // What each page set's pages are. Wikipedia's sets are named by language
  // (wikipedia-en), and any set not named here holds plain web pages.
  const KINDS = {
    papers: "paper",
    books: "book",
    stackoverflow: "question",
    stackexchange: "question",
    github: "repository",
    packages: "package",
    docs: "docs",
    podcasts: "podcast",
    music: "music",
    films: "film",
  };

  // A venue named like this is a conference's, not a journal's.
  const CONFERENCE = /\b(conference|proceedings|symposium|workshop|meeting)\b/i;

  // The item types made here that have no Library Catalog field.
  const NO_CATALOG = new Set(["webpage", "forumPost", "podcast"]);

  // arXiv's own DOIs, which OpenAlex gives the papers it holds there.
  const ARXIV_DOI = /^10\.48550\/arxiv\.(.+)$/i;

  /** The node's address without a trailing slash, the default node when
   * `text` is empty, or null when it is not an http or https address. A
   * path is kept, for a node served under one. Typed without a scheme, a
   * node at an IP address or on the local network is reached over http, as
   * nodes serve it, and one at a domain name over https. */
  function nodeAddress(text) {
    let value = String(text ?? "").trim() || DEFAULT_NODE;
    if (!value.includes("://")) {
      const host = parse(`http://${value}`)?.hostname ?? "";
      const plain = local(host) || /^[\d.]+$|^\[/.test(host);
      value = `${plain ? "http" : "https"}://${value}`;
    }
    const url = parse(value);
    if (!url || url.search || url.hash) return null;
    return url.origin + url.pathname.replace(/\/+$/, "");
  }

  /** The address that asks `node` for `query`: everything it lists, or
   * with `papers` only papers. Paper searches can also take years, but
   * plumbsearch.org's papers carry no dates yet, so none is asked for. */
  function searchURL(node, { query, limit, papers }) {
    const url = new URL(`${node}/api/search`);
    url.searchParams.set("q", query);
    url.searchParams.set("limit", String(limit));
    url.searchParams.set(...(papers ? ["kind", "paper"] : ["full", "1"]));
    return url.href;
  }

  /** The results of an answer in the order the node lists them, each
   * address once, the pages a site result holds right after it. */
  function results(answer) {
    const rows = answer?.assembled?.rows ?? [
      ...(answer?.pages ?? []).map((page) => ({ kind: "page", page })),
      ...(answer?.hits ?? []).map((site) => ({ kind: "site", site })),
    ];
    const found = [];
    const seen = new Set();
    const add = (result) => {
      if (result && !seen.has(result.url)) {
        seen.add(result.url);
        found.push(result);
      }
    };
    for (const row of rows) {
      if (row.kind == "page") {
        add(fromPage(row.page?.page));
      } else if (row.kind == "site") {
        add(fromSite(row.site));
        for (const page of row.pages ?? []) add(fromPage(page.page));
      }
    }
    return found;
  }

  /** How many rows the node listed. When that is as many as it was asked
   * for, a bigger limit may find more. */
  function listed(answer) {
    const { assembled, pages, hits } = answer ?? {};
    return assembled?.rows?.length ?? Math.max(pages?.length ?? 0, hits?.length ?? 0);
  }

  function fromPage(page) {
    const url = httpURL(page?.url);
    if (!url || !page.title) return null;
    const set = String(page.set ?? "");
    const kind = set.startsWith("wikipedia") ? "article" : (KINDS[set] ?? "page");
    return { kind, title: page.title, url, description: page.description ?? "", page };
  }

  function fromSite(site) {
    const url = httpURL(site?.url);
    if (!url) return null;
    const title = site.title || site.domain || url;
    return { kind: "site", title, url, description: site.description ?? "", site };
  }

  /** The DOI or arXiv ID Zotero can look a result up by, as
   * `Zotero.Translate.Search#setIdentifier` takes it, or null. */
  function identifier(result) {
    const paper = result.page?.paper ?? {};
    const doi = paper.doi || doiOf(result.url);
    if (doi) return { DOI: doi };
    const arxiv = paper.arxiv_id || arxivOf(result.url);
    return arxiv ? { arXiv: arxiv } : null;
  }

  /** A Zotero item, as JSON for `Zotero.Item#fromJSON`, from what the node
   * said of a result alone. */
  function item(result, now = new Date()) {
    const page = result.page ?? {};
    const by = byline(result.description);
    const json = {
      itemType: "webpage",
      title: result.title,
      url: result.url,
      accessDate: now.toISOString().replace(/\.\d+Z$/, "Z"),
      libraryCatalog: "Plumb Search",
    };
    switch (result.kind) {
      case "paper":
        Object.assign(json, paperFields(result, by));
        break;
      case "book":
        Object.assign(json, { itemType: "book", creators: people([by.who], "author"), date: by.year });
        break;
      case "article":
        Object.assign(json, {
          itemType: "encyclopediaArticle",
          encyclopediaTitle: "Wikipedia",
          abstractNote: page.lead || result.description,
          language: /^wikipedia-([a-z-]+)$/.exec(page.set)?.[1],
        });
        break;
      case "question":
        // Stack Overflow describes a question by its tags.
        Object.assign(json, {
          itemType: "forumPost",
          forumTitle: host(result.url) == "stackoverflow.com" ? "Stack Overflow" : host(result.url),
          postType: "Question",
          tags: result.description
            .split(", ")
            .filter(Boolean)
            .map((tag) => ({ tag, type: 1 })),
        });
        break;
      case "repository":
        Object.assign(json, {
          itemType: "computerProgram",
          abstractNote: result.description,
          creators: people([new URL(result.url).pathname.split("/")[1]], "programmer", true),
        });
        break;
      case "package":
        Object.assign(json, {
          itemType: "computerProgram",
          abstractNote: result.description,
          versionNumber: page.package?.version,
          date: page.package?.released,
          rights: page.package?.license,
        });
        break;
      case "podcast":
        Object.assign(json, { itemType: "podcast", creators: people([by.who], "podcaster", true) });
        break;
      case "music":
        Object.assign(json, {
          itemType: "audioRecording",
          creators: people([by.who], "performer", true),
          date: by.year,
        });
        break;
      case "film":
        Object.assign(json, { itemType: "film", abstractNote: result.description });
        break;
      default:
        Object.assign(json, {
          websiteTitle: result.site?.domain ?? page.site ?? host(result.url),
          abstractNote: result.description,
        });
    }
    if (NO_CATALOG.has(json.itemType)) delete json.libraryCatalog;
    for (const [field, value] of Object.entries(json)) {
      if (value == null || value === "" || (Array.isArray(value) && !value.length)) delete json[field];
    }
    return json;
  }

  // A paper from OpenAlex: a journal article, a conference paper or, on
  // arXiv with no other DOI, a preprint. Newer page sets carry the paper's
  // metadata; older ones only the description "Paper by A et al., 2017,
  // Venue", which names the first author alone.
  function paperFields(result, by) {
    const paper = result.page?.paper ?? {};
    const venue = paper.venue || by.venue;
    const doi = paper.doi || doiOf(result.url);
    const arxiv = paper.arxiv_id || arxivOf(result.url) || ARXIV_DOI.exec(doi ?? "")?.[1];
    const fields = {
      itemType: "journalArticle",
      creators: people(paper.authors?.length ? paper.authors : [by.who], "author"),
      date: paper.publication_date || String(paper.publication_year ?? by.year ?? ""),
      DOI: doi,
    };
    if (arxiv && (!doi || ARXIV_DOI.test(doi))) {
      return { ...fields, itemType: "preprint", repository: "arXiv", archiveID: `arXiv:${arxiv}` };
    }
    if (arxiv) fields.extra = `arXiv: ${arxiv}`;
    if (venue && CONFERENCE.test(venue)) {
      return { ...fields, itemType: "conferencePaper", proceedingsTitle: venue };
    }
    return { ...fields, publicationTitle: venue };
  }

  /** What a page set's description says of who made it, when, and where it
   * came out: "Paper by A et al., 2017, Venue", "Book by A, 1965", "Book,
   * 1965", "Song by A, 1975", "Podcast by A · Category". */
  function byline(description) {
    const text = String(description ?? "");
    let match = /^Paper(?: by (.+?)(?: et al\.)?)?(?:, (\d{4}))?(?:, (.+))?$/.exec(text);
    if (match) return { who: match[1], year: match[2], venue: match[3] };
    match = /^(?:Book|Song|Album|Podcast)(?: by (.+?))?(?:, (\d{4}))?$/.exec(text);
    return match ? { who: match[1]?.split(" · ")[0], year: match[2] } : {};
  }

  // Zotero creators from names: "First Last" or "Last, First" split in two,
  // a suffix ("Jr.") kept with the first name as Zotero's styles read it,
  // and a band, a company or a single word kept whole.
  function people(names, creatorType, whole = false) {
    return names
      .map((name) => String(name ?? "").trim())
      .filter(Boolean)
      .map((name) => {
        const [, rest, suffix] = /^(.+?)(?:,? (Jr\.?|Sr\.?|II|III|IV))?$/.exec(name);
        const comma = rest.indexOf(", ");
        const space = rest.lastIndexOf(" ");
        if (whole || (comma < 0 && space < 0)) return { creatorType, name };
        const [lastName, firstName] =
          comma > 0 ? [rest.slice(0, comma), rest.slice(comma + 2)] : [rest.slice(space + 1), rest.slice(0, space)];
        return { creatorType, firstName: suffix ? `${firstName}, ${suffix}` : firstName, lastName };
      });
  }

  // The DOI or arXiv ID in an address, without its query or fragment.
  function doiOf(text) {
    const url = parse(text ?? "");
    const doi = url && /^(?:dx\.|www\.)?doi\.org$/i.test(url.hostname) && /^\/(10\.\d{4,9}\/.+)$/.exec(url.pathname);
    return doi ? decode(doi[1]) : null;
  }

  function arxivOf(text) {
    const url = parse(text ?? "");
    const id =
      url && /^(?:www\.)?arxiv\.org$/i.test(url.hostname) && /^\/(?:abs|pdf)\/(.+?)(?:\.pdf)?$/.exec(url.pathname);
    return id ? decode(id[1]) : null;
  }

  /** Whether a host is this computer or on a private network: a name with
   * no dot, a .local or .localhost name, or a loopback, private or
   * link-local address. */
  function local(hostname) {
    const host = hostname.toLowerCase().replace(/^\[|\]$/g, "");
    if (host.includes(":")) return host == "::1" || host == "::" || /^(f[cd]|fe[89ab]|::ffff:)/.test(host);
    if (!host.includes(".") || /\.(local|localhost)$/.test(host)) return true;
    const ip = /^(\d+)\.(\d+)\.\d+\.\d+$/.exec(host);
    if (!ip) return false;
    const [a, b] = [Number(ip[1]), Number(ip[2])];
    return (
      [0, 10, 127].includes(a) || (a == 172 && b >= 16 && b < 32) || (a == 192 && b == 168) || (a == 169 && b == 254)
    );
  }

  function decode(text) {
    try {
      return decodeURIComponent(text);
    } catch {
      return text;
    }
  }

  function parse(text) {
    try {
      const url = new URL(text);
      return /^https?:$/.test(url.protocol) ? url : null;
    } catch {
      return null;
    }
  }

  // The address as the node gave it, when it is an http or https one.
  function httpURL(text) {
    return typeof text == "string" && parse(text.trim()) ? text.trim() : null;
  }

  function host(url) {
    return new URL(url).hostname.replace(/^www\./, "");
  }

  return { DEFAULT_NODE, nodeAddress, searchURL, results, listed, identifier, item, local };
})();

if (typeof module == "object") module.exports = Plumb;

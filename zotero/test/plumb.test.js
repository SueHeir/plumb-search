// Run with `node --test zotero/test/plumb.test.js`. The fixtures are
// answers from plumbsearch.org, cut to the fields the plugin reads.

const assert = require("node:assert/strict");
const { test } = require("node:test");
const Plumb = require("../content/plumb.js");

const fixture = (name) => require(`./fixtures/${name}.json`);
const NOW = new Date("2026-10-13T19:40:00.123Z");

test("node addresses", () => {
  assert.equal(Plumb.nodeAddress(""), "https://plumbsearch.org");
  assert.equal(Plumb.nodeAddress(" https://plumbsearch.org/ "), "https://plumbsearch.org");
  assert.equal(Plumb.nodeAddress("plumbsearch.org"), "https://plumbsearch.org");
  assert.equal(Plumb.nodeAddress("127.0.0.1:7586"), "http://127.0.0.1:7586");
  assert.equal(Plumb.nodeAddress("localhost:8080/"), "http://localhost:8080");
  assert.equal(Plumb.nodeAddress("homelab.local:8080"), "http://homelab.local:8080");
  assert.equal(Plumb.nodeAddress("homelab:8080"), "http://homelab:8080");
  assert.equal(Plumb.nodeAddress("203.0.113.7:8080"), "http://203.0.113.7:8080");
  assert.equal(Plumb.nodeAddress("[::1]:8080"), "http://[::1]:8080");
  assert.equal(Plumb.nodeAddress("https://example.org/plumb/"), "https://example.org/plumb");
  assert.equal(Plumb.nodeAddress("ftp://example.org"), null);
  assert.equal(Plumb.nodeAddress("javascript:alert(1)"), null);
  assert.equal(Plumb.nodeAddress("https://example.org/?q=x"), null);
});

test("search addresses", () => {
  const node = "https://example.org/plumb";
  assert.equal(
    Plumb.searchURL(node, { query: "marie curie", limit: 25 }),
    "https://example.org/plumb/api/search?q=marie+curie&limit=25&full=1",
  );
  assert.equal(
    Plumb.searchURL(node, { query: "rna", limit: 50, papers: true }),
    "https://example.org/plumb/api/search?q=rna&limit=50&kind=paper",
  );
});

test("results keep the node's order, with a site's own pages after it", () => {
  const listed = (name) => Plumb.results(fixture(name)).map((r) => `${r.kind} ${r.url}`);
  assert.deepEqual(listed("everything"), [
    "article https://en.wikipedia.org/wiki/Attention_Is_All_You_Need",
    "paper https://doi.org/10.65215/2q58a426",
    "site https://you.com/",
    "site https://all.biz/",
  ]);
  assert.deepEqual(listed("books"), [
    "site https://dunenovels.com/",
    "article https://en.wikipedia.org/wiki/Dune_(novel)",
    "article https://en.wikipedia.org/wiki/Children_of_Dune",
    "question https://english.stackexchange.com/questions/360106",
  ]);
  // A paper search answers with pages alone.
  assert.deepEqual(listed("papers"), [
    "paper https://doi.org/10.18653/v1/p19-1285",
    "paper https://doi.org/10.1093/bioinformatics/btab083",
    "paper https://doi.org/10.1016/j.jml.2007.11.007",
  ]);
});

test("results drop repeats and addresses that are not http or https", () => {
  const page = (url) => ({ page: { set: "docs", url, title: "T" } });
  const answer = {
    hits: [{ domain: "a.org", url: "https://a.org/", title: "A" }],
    pages: [page("https://a.org/"), page("javascript:alert(1)"), page("file:///etc/passwd")],
  };
  assert.deepEqual(
    Plumb.results(answer).map((r) => r.url),
    ["https://a.org/"],
  );
  assert.deepEqual(Plumb.results(null), []);
  assert.deepEqual(Plumb.results({ error: "search failed" }), []);
});

test("more results: when the node listed as many as it was asked for", () => {
  assert.equal(Plumb.listed(fixture("everything")), 4);
  assert.equal(Plumb.listed(fixture("papers")), 3);
  assert.equal(Plumb.listed({}), 0);
});

test("pages on this computer or the local network", () => {
  const local = "localhost homelab nas.local 127.0.0.1 10.1.2.3 172.20.0.1 192.168.1.1 169.254.1.1 [::1] [fd00::1]";
  for (const host of local.split(" ")) assert.equal(Plumb.local(host), true, host);
  for (const host of ["plumbsearch.org", "8.8.8.8", "172.32.0.1", "[2606:4700::1111]"]) {
    assert.equal(Plumb.local(host), false, host);
  }
});

test("papers are looked up by DOI or arXiv ID", () => {
  const [paper] = Plumb.results(fixture("papers"));
  assert.deepEqual(Plumb.identifier(paper), { DOI: "10.18653/v1/p19-1285" });
  const arxiv = { kind: "paper", url: "https://arxiv.org/abs/1706.03762v7?context=cs", page: {} };
  assert.deepEqual(Plumb.identifier(arxiv), { arXiv: "1706.03762v7" });
  const doi = { kind: "paper", url: "https://doi.org/10.1000/182#section", page: {} };
  assert.deepEqual(Plumb.identifier(doi), { DOI: "10.1000/182" });
  const site = Plumb.results(fixture("everything")).find((r) => r.kind == "site");
  assert.equal(Plumb.identifier(site), null);
});

test("a paper from its description", () => {
  const [conference, journal, alone] = Plumb.results(fixture("papers")).map((r) => Plumb.item(r, NOW));
  assert.deepEqual(conference, {
    itemType: "conferencePaper",
    title: "Transformer-XL: Attentive Language Models beyond a Fixed-Length Context",
    url: "https://doi.org/10.18653/v1/p19-1285",
    accessDate: "2026-10-13T19:40:00Z",
    libraryCatalog: "Plumb Search",
    creators: [{ creatorType: "author", firstName: "Zihang", lastName: "Dai" }],
    date: "2019",
    DOI: "10.18653/v1/p19-1285",
    proceedingsTitle: "Annual Meeting of the Association for Computational Linguistics (ACL)",
  });
  assert.equal(journal.itemType, "journalArticle");
  assert.equal(journal.publicationTitle, "Bioinformatics");
  assert.deepEqual(alone.creators, [{ creatorType: "author", firstName: "T. Florian", lastName: "Jaeger" }]);
  assert.equal(alone.date, "2008");
});

test("a paper from its metadata", () => {
  const result = {
    kind: "paper",
    title: "Attention Is All You Need",
    url: "https://doi.org/10.48550/arxiv.1706.03762",
    description: "Paper by Ashish Vaswani et al., 2017, arXiv (Cornell University)",
    page: {
      set: "papers",
      paper: {
        doi: "10.48550/arXiv.1706.03762",
        arxiv_id: "1706.03762",
        authors: ["Ashish Vaswani", "Noam Shazeer", "Niki Parmar"],
        publication_date: "2017-06-12",
        publication_year: 2017,
        venue: "arXiv (Cornell University)",
      },
    },
  };
  // On arXiv, with arXiv's DOI, it is a preprint.
  const json = Plumb.item(result, NOW);
  assert.equal(json.itemType, "preprint");
  assert.equal(json.repository, "arXiv");
  assert.equal(json.archiveID, "arXiv:1706.03762");
  assert.equal(json.DOI, "10.48550/arXiv.1706.03762");
  assert.equal(json.publicationTitle, undefined);
  assert.equal(json.date, "2017-06-12");
  assert.deepEqual(
    json.creators.map((c) => c.lastName),
    ["Vaswani", "Shazeer", "Parmar"],
  );
  // So it is with only its arXiv address.
  const bare = Plumb.item({ ...result, url: "https://arxiv.org/abs/1706.03762", page: { set: "papers" } }, NOW);
  assert.equal(bare.itemType, "preprint");
  assert.equal(bare.archiveID, "arXiv:1706.03762");
  assert.equal(bare.DOI, undefined);
  // Published with a DOI of its own, it is a journal article.
  const paper = {
    ...result.page.paper,
    doi: "10.5555/3295222.3295349",
    venue: "Neural Information Processing Systems",
  };
  const published = Plumb.item({ ...result, page: { set: "papers", paper } }, NOW);
  assert.equal(published.itemType, "journalArticle");
  assert.equal(published.DOI, "10.5555/3295222.3295349");
  assert.equal(published.extra, "arXiv: 1706.03762");
});

test("a Wikipedia article", () => {
  const [article] = Plumb.results(fixture("everything"));
  const json = Plumb.item(article, NOW);
  assert.equal(json.itemType, "encyclopediaArticle");
  assert.equal(json.encyclopediaTitle, "Wikipedia");
  assert.equal(json.language, "en");
  assert.match(json.abstractNote, /^"Attention Is All You Need" is a 2017 research paper/);
});

test("a book", () => {
  const book = {
    kind: "book",
    title: "Dune",
    url: "https://openlibrary.org/works/OL893415W",
    description: "Book by Frank Herbert, 1965",
    page: { set: "books" },
  };
  assert.deepEqual(Plumb.item(book, NOW), {
    itemType: "book",
    title: "Dune",
    url: "https://openlibrary.org/works/OL893415W",
    accessDate: "2026-10-13T19:40:00Z",
    libraryCatalog: "Plumb Search",
    creators: [{ creatorType: "author", firstName: "Frank", lastName: "Herbert" }],
    date: "1965",
  });
  const unsigned = Plumb.item({ ...book, description: "Book, 1965" }, NOW);
  assert.equal(unsigned.date, "1965");
  assert.equal(unsigned.creators, undefined);
  const king = Plumb.item({ ...book, description: "Book by Martin Luther King, Jr., 1964" }, NOW);
  assert.deepEqual(king.creators, [{ creatorType: "author", firstName: "Martin Luther, Jr.", lastName: "King" }]);
  assert.equal(king.date, "1964");
});

test("a question, a package and a site", () => {
  const [question] = Plumb.results(fixture("questions"));
  const json = Plumb.item(question, NOW);
  assert.equal(json.itemType, "forumPost");
  assert.equal(json.forumTitle, "Stack Overflow");
  assert.deepEqual(
    json.tags.map((t) => t.tag),
    ["git", "version-control", "git-commit", "undo"],
  );
  assert.equal(json.libraryCatalog, undefined);

  const [pypi] = Plumb.results(fixture("packages"));
  const program = Plumb.item(pypi, NOW);
  assert.equal(program.itemType, "computerProgram");
  assert.equal(program.versionNumber, "2.5.3");
  assert.equal(program.date, "2026-09-06");

  const site = Plumb.results(fixture("packages")).find((r) => r.kind == "site");
  assert.deepEqual(Plumb.item(site, NOW), {
    itemType: "webpage",
    title: "NumPy",
    url: "https://numpy.org/",
    accessDate: "2026-10-13T19:40:00Z",
    websiteTitle: "numpy.org",
    abstractNote: site.description,
  });
});

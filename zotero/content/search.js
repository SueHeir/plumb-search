/* The search window: asks the Plumb node set in Zotero's settings, lists
 * what it found, and adds the chosen results to the library or collection
 * selected in Zotero's main window.
 */

const PREF = "extensions.plumb-search.";
const HTML = "http://www.w3.org/1999/xhtml";
// How many results a search asks for, then each "More results".
const LIMITS = [25, 50, 100];

// What the last search listed: { result, box, status, state } for each row.
let shown = [];
// The search that listed them: { query, papers, limit }, for More results.
let last = null;
// The latest search: an older one that answers late is dropped.
let ticket = 0;
// Whether results are being added, when Add stays off.
let saving = false;

const $ = (id) => document.getElementById(id);

window.addEventListener("load", () => {
  $("query").addEventListener("keydown", (event) => event.key == "Enter" && searchBox());
  $("go").addEventListener("click", searchBox);
  $("papers").addEventListener("change", searchBox);
  $("more").addEventListener("click", () => search({ ...last, limit: LIMITS[LIMITS.indexOf(last.limit) + 1] }));
  $("save").addEventListener("click", save);
  $("results").addEventListener("change", updateSave);
  window.addEventListener("keydown", (event) => event.key == "Escape" && window.close());
  window.addEventListener("focus", showTarget);
  showTarget();
  PlumbSearch.start(window.arguments?.[0]);
});

var PlumbSearch = {
  /** Runs a search the menus asked for, or waits for one. */
  start(args) {
    if (args?.query) {
      $("query").value = args.query;
      searchBox();
    }
    $("query").focus();
  },
};

// Searches for what the search box holds.
function searchBox() {
  search({ query: $("query").value.trim(), papers: $("papers").checked, limit: LIMITS[0] });
}

async function search(asked) {
  if (!asked.query || !asked.limit) return;
  const node = Plumb.nodeAddress(Zotero.Prefs.get(PREF + "node", true));
  if (!node) return say("plumb-search-bad-node");
  const url = Plumb.searchURL(node, asked);
  const mine = ++ticket;
  const where = { node: new URL(node).host };
  say("plumb-search-searching", where);
  showTarget();
  $("more").hidden = true;
  let answer;
  try {
    answer = await Zotero.HTTP.request("GET", url, {
      responseType: "json",
      headers: { Accept: "application/json" },
      successCodes: false,
    });
  } catch (error) {
    Zotero.debug(`Plumb Search: ${error}`);
  }
  if (mine != ticket) return;
  // Status 0: no answer at all, the connection failed or timed out.
  if (!answer?.status) return say("plumb-search-unreachable", where);
  if (answer.status == 503) return say("plumb-search-not-ready", where);
  if (answer.status != 200 || !answer.response) {
    return say("plumb-search-failed", { ...where, error: answer.response?.error ?? answer.status });
  }
  const results = Plumb.results(answer.response);
  last = asked;
  list(results);
  say("plumb-search-found", { ...where, count: results.length });
  $("more").hidden = Plumb.listed(answer.response) < asked.limit || asked.limit == LIMITS.at(-1);
}

// Lists `results`. A result already listed keeps its row's checkbox and
// state, so one being added goes on showing how that went.
function list(results) {
  const before = new Map(shown.map((entry) => [entry.result.url, entry]));
  shown = results.map((result) => {
    const entry = before.get(result.url) ?? { box: element("input"), status: element("span", "state") };
    entry.result = result;
    entry.box.type = "checkbox";
    return entry;
  });
  $("results").replaceChildren(...shown.map(row));
  $("results").scrollTop = 0;
  updateSave();
}

function row({ result, box, status }) {
  const kind = element("span", "kind");
  document.l10n.setAttributes(kind, `plumb-search-kind-${result.kind}`);
  const title = element("span", "title", result.title);
  const link = element("span", "link", result.url);
  link.tabIndex = 0;
  link.setAttribute("role", "link");
  document.l10n.setAttributes(link, "plumb-search-open-link");
  const open = (event) => {
    event.preventDefault();
    Zotero.launchURL(result.url);
  };
  link.addEventListener("click", open);
  link.addEventListener("keydown", (event) => event.key == "Enter" && open(event));
  const head = element("span", "head");
  head.append(kind, title);
  const text = element("span", "text");
  text.append(head);
  if (result.description) text.append(element("span", "about", result.description));
  text.append(link);
  const label = element("label");
  label.append(box, text, status);
  const item = element("li");
  item.append(label);
  return item;
}

async function save() {
  const chosen = shown.filter((entry) => entry.box.checked && !entry.box.disabled);
  if (saving || !chosen.length) return;
  const into = target();
  const lookup = Zotero.Prefs.get(PREF + "lookup", true);
  saving = true;
  for (const entry of chosen) entry.box.disabled = true;
  updateSave();
  say("plumb-search-saving", { where: into.name });
  let added = 0;
  for (const entry of chosen) {
    mark(entry, "saving");
    try {
      await add(entry.result, into, lookup);
      mark(entry, "added");
      added++;
    } catch (error) {
      Zotero.logError(error);
      mark(entry, "failed");
    }
  }
  if (added < chosen.length) say("plumb-search-save-failed");
  else say("plumb-search-saved", { count: added, where: into.name });
  saving = false;
  updateSave();
}

// Adds a result: looked up by its DOI or arXiv ID, or read from its page
// with Zotero's translators, when that is on and finds it; else made from
// what Plumb said of it.
async function add(result, into, lookup) {
  if (lookup) {
    const id = Plumb.identifier(result);
    const items = await (id ? byIdentifier(id, into) : byPage(result.url, into));
    if (items.length) return;
  }
  const json = Plumb.item(result);
  const item = new Zotero.Item(json.itemType);
  item.libraryID = into.libraryID;
  item.fromJSON(json);
  item.setCollections(into.collections);
  await item.saveTx();
}

async function byIdentifier(identifier, { libraryID, collections }) {
  try {
    const translate = new Zotero.Translate.Search();
    translate.setIdentifier(identifier);
    translate.setTranslator(await translate.getTranslators());
    return await translate.translate({ libraryID, collections, saveAttachments: true });
  } catch (error) {
    Zotero.debug(`Plumb Search: no item for ${JSON.stringify(identifier)}: ${error}`);
    return [];
  }
}

// The page's own item, as the Zotero Connector would save it. The page is
// read as a document, so none of its scripts run, and a page that lists
// several items (a site's front page, often) is left to the fallback. So is
// a page on this computer or the local network, which no public page set
// should list. Any status is taken as it comes: Zotero would otherwise retry
// a busy server for up to an hour while the other results wait.
async function byPage(url, { libraryID, collections }) {
  if (Plumb.local(new URL(url).hostname)) return [];
  try {
    const answer = await Zotero.HTTP.request("GET", url, {
      responseType: "document",
      timeout: 20000,
      successCodes: false,
    });
    if (answer.status != 200 || !answer.response) return [];
    const translate = new Zotero.Translate.Web();
    translate.setDocument(Zotero.HTTP.wrapDocument(answer.response, answer.responseURL));
    const translators = await translate.getTranslators(true);
    const single = translators.filter((translator) => translator.itemType != "multiple");
    if (!single.length) return [];
    translate.setTranslator(single);
    return await translate.translate({ libraryID, collections, saveAttachments: true });
  } catch (error) {
    Zotero.debug(`Plumb Search: no item from ${url}: ${error}`);
    return [];
  }
}

// Where results are added: the library and collections selected in the
// main window, or My Library when that library cannot be edited.
function target() {
  const pane = Zotero.getActiveZoteroPane();
  let libraryID = Zotero.Libraries.userLibraryID;
  let collections = [];
  if (pane) {
    // Zotero 10 can select several rows; Zotero 8 and 9 select one.
    libraryID = pane.getSelectedLibraryIDs?.()[0] ?? pane.getSelectedLibraryID();
    collections = pane.getSelectedCollections?.() ?? [pane.getSelectedCollection()];
  }
  let library = Zotero.Libraries.get(libraryID);
  if (!library?.editable || library.libraryType == "feed") {
    library = Zotero.Libraries.get(Zotero.Libraries.userLibraryID);
    collections = [];
  }
  collections = collections.filter((c) => c && c.libraryID == library.libraryID);
  return {
    libraryID: library.libraryID,
    collections: collections.map((c) => c.id),
    name: collections.map((c) => c.name).join(", ") || library.name,
  };
}

function showTarget() {
  document.l10n.setAttributes($("into"), "plumb-search-into", { where: target().name });
}

function updateSave() {
  const count = shown.filter((entry) => entry.box.checked && !entry.box.disabled).length;
  document.l10n.setAttributes($("save"), "plumb-search-save", { count });
  $("save").disabled = saving || !count;
}

function mark(entry, state) {
  entry.state = state;
  entry.box.disabled = state == "saving" || state == "added";
  if (state == "added") entry.box.checked = false;
  if (state) document.l10n.setAttributes(entry.status, `plumb-search-state-${state}`);
  entry.status.dataset.state = state ?? "";
}

function say(id, args) {
  document.l10n.setAttributes($("status"), id, args);
}

function element(name, className, text) {
  const node = document.createElementNS(HTML, name);
  if (className) node.className = className;
  if (text) node.textContent = text;
  return node;
}

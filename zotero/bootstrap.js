/* Plumb Search for Zotero: registers the search window, its menu items and
 * its settings pane when the plugin starts, and removes them when it stops.
 */

var chromeHandle;

function install() {}

function uninstall() {}

function startup({ id, rootURI }) {
  const aomStartup = Cc["@mozilla.org/addons/addon-manager-startup;1"].getService(Ci.amIAddonManagerStartup);
  chromeHandle = aomStartup.registerChrome(Services.io.newURI(rootURI + "manifest.json"), [
    ["content", "plumb-search", "content/"],
  ]);
  Zotero.PreferencePanes.register({
    pluginID: id,
    src: rootURI + "content/preferences.xhtml",
    image: rootURI + "icon.svg",
  });
  Zotero.MenuManager.registerMenu({
    menuID: "plumb-search-tools",
    pluginID: id,
    target: "main/menubar/tools",
    menus: [
      {
        menuType: "menuitem",
        l10nID: "plumb-search-menu-search",
        onCommand: (event) => openSearch(event.target.ownerGlobal, {}),
      },
    ],
  });
  Zotero.MenuManager.registerMenu({
    menuID: "plumb-search-item",
    pluginID: id,
    target: "main/library/item",
    menus: [
      {
        menuType: "menuitem",
        l10nID: "plumb-search-menu-item",
        onShowing: (event, context) =>
          context.setVisible(context.items?.length == 1 && context.items[0].isRegularItem()),
        onCommand: (event, context) => openSearch(event.target.ownerGlobal, { query: queryFor(context.items[0]) }),
      },
    ],
  });
  for (const window of Zotero.getMainWindows()) onMainWindowLoad({ window });
}

function onMainWindowLoad({ window }) {
  window.MozXULElement.insertFTLIfNeeded("plumb-search.ftl");
}

function onMainWindowUnload() {}

function shutdown() {
  for (const window of Services.wm.getEnumerator("plumb-search:search")) window.close();
  for (const window of Zotero.getMainWindows()) {
    window.document.querySelector('[href="plumb-search.ftl"]')?.remove();
    // Zotero 8 leaves a stopped plugin's menu items in the menus.
    for (const item of window.document.querySelectorAll('[data-l10n-id^="plumb-search-menu"]')) item.remove();
  }
  chromeHandle?.destruct();
  chromeHandle = null;
}

// One search window: a second search from the menus goes to the open one.
function openSearch(window, args) {
  const open = Services.wm.getMostRecentWindow("plumb-search:search");
  if (open) {
    open.PlumbSearch.start(args);
    open.focus();
    return;
  }
  window.openDialog("chrome://plumb-search/content/search.xhtml", "", "chrome,dialog=no,resizable,centerscreen", args);
}

// What to search for an item: its title (a case's name, an email's subject),
// and a book's first author with it, as Plumb finds books ("dune herbert").
function queryFor(item) {
  const title = item.getField("title", false, true);
  const author = item.itemType == "book" && item.getCreator(0)?.lastName;
  return author ? `${title} ${author}` : title;
}

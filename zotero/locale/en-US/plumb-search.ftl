## Menus in Zotero's main window

plumb-search-menu-search =
    .label = Search Plumb…
plumb-search-menu-item =
    .label = Search Plumb for This Item

## The search window

plumb-search-window =
    .title = Plumb Search
plumb-search-query =
    .placeholder = Search for papers, books, articles or sites
    .aria-label = Search Plumb
plumb-search-go = Search
plumb-search-papers = Papers only
plumb-search-more = More results
plumb-search-save =
    { $count ->
        [0] Add to Zotero
        [one] Add 1 to Zotero
       *[other] Add { $count } to Zotero
    }
plumb-search-into = Adds to { $where }
plumb-search-open-link =
    .title = Open in your browser

plumb-search-searching = Searching { $node }…
plumb-search-found =
    { $count ->
        [0] { $node } found nothing for this search.
        [one] One result from { $node }.
       *[other] { $count } results from { $node }.
    }
plumb-search-not-ready = { $node } is still building its index. Try again in a minute.
plumb-search-failed = { $node } could not search: { $error }
plumb-search-unreachable = Could not reach { $node }. The node is set in Zotero’s settings, under Plumb Search.
plumb-search-bad-node = The Plumb node in Zotero’s settings is not a web address.
plumb-search-saving = Adding to { $where }…
plumb-search-saved =
    { $count ->
        [one] Added 1 item to { $where }.
       *[other] Added { $count } items to { $where }.
    }
plumb-search-save-failed = Some could not be added. Zotero’s Help menu has the error log.

plumb-search-state-saving = Adding…
plumb-search-state-added = Added
plumb-search-state-failed = Not added

## What each result is

plumb-search-kind-paper = Paper
plumb-search-kind-book = Book
plumb-search-kind-article = Wikipedia
plumb-search-kind-question = Q&A
plumb-search-kind-repository = Code
plumb-search-kind-package = Package
plumb-search-kind-docs = Docs
plumb-search-kind-podcast = Podcast
plumb-search-kind-music = Music
plumb-search-kind-film = Film
plumb-search-kind-page = Page
plumb-search-kind-site = Site

## The settings pane

plumb-search-pref-node = Plumb node
plumb-search-pref-node-help = Searches go to this node: plumbsearch.org, or your own, such as http://127.0.0.1:7586 for the desktop app or http://127.0.0.1:8080 for Docker and plumb run.
plumb-search-pref-adding = Adding items
plumb-search-pref-lookup =
    .label = Look up full details when adding
plumb-search-pref-lookup-help = Papers are looked up by their DOI or arXiv ID, and other pages are read with Zotero’s translators, as Add Item by Identifier and the Zotero Connector do. Without this, items are made from what Plumb returned, and Zotero contacts no other site.

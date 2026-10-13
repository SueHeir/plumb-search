#!/bin/sh
# Packs the plugin into target/zotero/plumb-search-<version>.xpi, the file
# Zotero installs (Tools > Plugins > Install Plugin From File).
set -eu
cd "$(dirname "$0")"
version=$(node -p 'require("./manifest.json").version')
xpi="../target/zotero/plumb-search-$version.xpi"
mkdir -p ../target/zotero
rm -f "$xpi"
zip -qrX "$xpi" manifest.json bootstrap.js prefs.js icon.svg content locale
echo "$xpi"

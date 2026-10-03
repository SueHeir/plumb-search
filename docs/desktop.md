# The Plumb Search desktop app

The desktop app runs a Plumb Search node on your computer. Search opens in
your browser at `http://127.0.0.1:7586`; the app window manages the node.
It shares its panel and feature settings with Docker, from this repository.

The panel has these sections:

- **Overview:** search readiness, crawl progress, storage, downloads, and live peer status.
- **Search & browser:** browser setup, search by meaning, and private browser search.
- **Resources:** background crawling and daily download/storage limits; changes apply immediately.
- **Network & privacy:** joining the network, bootstrap nodes, trusted nodes (plumbsearch.org's crawler by default), anonymous popularity sharing, crawl agreement, and relay activity.
- **Remote control:** let the app on another computer control this node.
- **About:** version, data location, diagnostics, and source code.

The desktop joins the network by default. Save feature choices, then use **Restart to apply** when offered. Otherwise use **Quit Plumb Search** in the tray/menu bar and reopen the app, or restart the Docker container. Closing the window keeps the node running. A banner shows
when saved choices differ from the running node. Resource and feature forms
never refresh automatically while you edit them.

Meaning search downloads a model and builds vectors in the background.
Private browser search needs the bundled WebAssembly module and an index
with buckets; the panel distinguishes disabled, preparing, and unavailable
states. Desktop installers build and include that module.

Feature choices are stored in `features.json` in the data folder and take
precedence over startup feature defaults. Delete that file while stopped to
return to defaults. Server transport/relay flags are preserved. For an isolated
development or test node, set `PLUMB_DESKTOP_DATA_DIR` to a separate folder
before launching the desktop executable.

Every link out of the panel opens in your default browser. Closing the window
keeps the app running, so searches from your browser keep working: it stays
in the menu bar on macOS and in the notification area on Windows and Linux.
Its icon there has **Open Plumb Search**, **Start at login** and **Quit
Plumb Search**. With **Start at login** checked, the app starts in the
background when you log in, without opening its window, so your browser's
searches work from the start. Starting
the app again while it is running brings its window back too, and so does
clicking its Dock icon on macOS.

Test builds are not code-signed yet, so each system warns you before it opens
them the first time. The [install](#install) steps say how to get past that.

## Control your other nodes

Above the panel, **This computer** is the app's own node. **+ Connect to a
node** adds another one, such as a Docker container or homelab server, so the
app becomes the control center for all of them:

1. Turn on remote control on that node. In its own panel (opened on its
   computer), go to **Remote control** and click **Turn on and make a token**.
   For Docker, run `docker exec <container> plumb remote-control on` on its
   host. Either way you get a token, shown once.
2. In the app, click **+ Connect to a node** and enter the node's address
   (such as `http://192.168.1.20:8080`) and the token.

The node then has its own tab with the same sections. Its forms change that
node; if it cannot be reached, or its token was replaced, the tab says why
and offers to connect again or forget it. Tokens are kept in
`remote-nodes.json` in the app's data folder, readable only by you, and the
app's own node sends them; the window never sees them. A node takes remote
control only from its own computer and local networks unless its owner
allows more (see [Docker](docker.md#control-it-from-the-desktop-app)).

The app's own node listens only on this computer, so other computers cannot
control it.

## Get a test build

Every pull request builds the installers in the **Desktop** workflow:

1. Open the pull request's **Checks** tab, or the repository's **Actions**
   tab, and pick the latest **Desktop** run.
2. On the run's **Summary** page, under **Artifacts**, download the one for
   your system. Downloading artifacts needs a signed-in GitHub account.
3. Unzip the download to get the installer.

| Artifact | Contents |
| --- | --- |
| `plumb-search-windows-x64` | `plumb-search_<version>_x64-setup.exe` and `plumb-search_<version>_x64_en-US.msi` |
| `plumb-search-macos-universal` | `plumb-search_<version>_universal.dmg`, for Apple silicon and Intel Macs |
| `plumb-search-linux-x86_64` | `plumb-search_<version>_amd64.deb`, `plumb-search-<version>-1.x86_64.rpm` and `plumb-search_<version>_amd64.AppImage` |

To build another branch, open **Actions > Desktop > Run workflow** and pick
the branch. Pushing a tag such as `v0.1.0` also attaches the installers to a
draft release on the **Releases** page, where they can be downloaded without
an account once the release is published. The installers take their version
from `version` in the root `Cargo.toml`, so update it before tagging.

## Install

### Windows

Run `plumb-search_<version>_x64-setup.exe`. It installs the app for your user
account and needs no administrator rights. The `.msi` installs it for every
user of the computer and asks for administrator rights; use one or the other.

Windows SmartScreen may stop the installer with "Windows protected your PC".
Click **More info**, then **Run anyway**.

The app needs Microsoft Edge WebView2, which Windows 10 and 11 normally have;
if it is missing, the installer downloads it. To uninstall, use **Settings >
Apps > Installed apps > Plumb Search**.

### macOS

Open the `.dmg` and drag **Plumb Search** into **Applications**.

The app is not notarized by Apple, so macOS refuses to open it the first time.
Do one of these once:

- **macOS 14 and earlier:** in Finder, open **Applications**, right-click (or
  Control-click) **Plumb Search**, choose **Open**, then **Open** again.
- **macOS 15 and later:** open the app and click **Done** when macOS refuses.
  Then go to **System Settings > Privacy & Security**, scroll to the message
  about Plumb Search, click **Open Anyway** and confirm.
- **Any version, in Terminal:** remove the quarantine flag that macOS put on the
  download. This also fixes "Plumb Search is damaged and can't be opened".

  ```sh
  xattr -dr com.apple.quarantine "/Applications/Plumb Search.app"
  ```

To uninstall, drag the app to the Trash, and delete the data folder (see
[Where data lives](#where-data-lives)) if you no longer want the index.

### Linux

Pick the package for your distribution. Each one adds **Plumb Search** to the
application menu, and the command `plumb-desktop` starts it from a terminal.

- **Debian, Ubuntu and derivatives:** install the `.deb` with apt, which also
  installs the libraries it needs (WebKitGTK):

  ```sh
  sudo apt install ./plumb-search_<version>_amd64.deb
  ```

- **Fedora, RHEL, openSUSE:** install the `.rpm`:

  ```sh
  sudo dnf install ./plumb-search-<version>-1.x86_64.rpm
  ```

  The package is not signed; if dnf refuses it, add `--nogpgcheck`.

- **Any distribution:** the `.AppImage` runs without installing. Downloads
  lose their executable bit, so set it first:

  ```sh
  chmod +x plumb-search_<version>_amd64.AppImage
  ./plumb-search_<version>_amd64.AppImage
  ```

  AppImages need FUSE 2. If it is missing (`libfuse.so.2` errors), install it
  (`sudo apt install libfuse2t64` on Ubuntu 24.04, `libfuse2` on older
  releases) or run the AppImage with `--appimage-extract-and-run`.

Uninstall with `sudo apt remove plumb-search` or `sudo dnf remove plumb-search`,
or delete the AppImage.

## First launch

The window opens with "Starting..." and then shows the panel. On the first
launch the node sets up its index in the background. **This needs an internet
connection.**

1. It downloads the Tranco list of popular sites (from tranco-list.eu) and
   builds a first index of the best-ranked 250,000 of them. That takes a
   minute or two, and the panel then says "Limited search is ready".
2. It downloads the official websites listed in Wikidata (from
   query.wikidata.org), which takes several minutes, folds them in and
   swaps in a better index. The panel then says "Search is ready".
3. It crawls the homepages of the top 2,000 sites to fill in titles and
   descriptions, then keeps crawling 1,000 more every 12 hours while the app
   runs. The setting "Keep the index up to date in the background" turns
   this off (and back on).

The app starts with a download limit of 500 MB a day and a storage limit of
2,000 MB, which the panel's settings change (empty for no limit). Once a
day's downloads reach the limit, crawling pauses until the next day (UTC);
while the data folder is over its limit, crawling pauses. Setup's own
downloads count toward the day but are never held back, and search keeps
working either way.

The Resources page also has one Workload choice, which sets the pace and
both limits at once: Light (4 homepages at a time, 100 MB a day, 1 GB of
disk), Balanced (the defaults: 16 at a time, 500 MB, 2 GB), Full (32 at a
time, no limits) or Custom (the limits as typed). "Only crawl between"
keeps crawling to some hours of the computer's clock, such as 22:00 to
07:00. The Crawling card has "Pause for an hour" and "Pause until tomorrow"
(06:00), and "Resume now" while paused.

Feature changes (the network, search by meaning, private search) apply when
the node starts. In the app the panel then offers "Restart to apply", which
stops the node and starts it again without quitting the app; a Docker node
needs its container restarted. While search by meaning downloads its model
or makes site vectors, the panel shows how far it has come.

The Activity section is the node's log in plain sentences: starts and
stops, crawl rounds and what they found, index rebuilds, settings changes,
and every failure, newest first (the last few hundred entries, kept in
`activity.jsonl` in the data folder). Failures on the panel come with "Try
again now": the failed update, Wikidata's official websites and search by
meaning each have one, and the network card has its own.

The Backup section saves what cannot be downloaded again: the settings,
the node's network identity and crawl credits, and the remote control
files. Sites and the index are left out, since a node rebuilds them.
Backups are kept in `backups/` in the data folder (the last 10), can be
downloaded, and restore from that list or from a file. Restoring first
saves the current files as a backup marked `before-restore`, then restarts
the node. Backups hold the node's keys, so keep them private.

If a download fails, for example without internet access or behind a
firewall that blocks those sites, the error appears on the panel.

The app has no proxy setting yet. Its downloads go through a proxy only when
one is set in the `HTTPS_PROXY` or `ALL_PROXY` environment variable of the
app (it does not read the system's proxy settings), and it always fetches
homepages directly. So on a network that reaches the internet only through a
proxy, its crawls fail: the search page says that the last update failed,
and `http://127.0.0.1:7586/api/status` suggests `--use-system-proxy`, which
is an option of the command-line node (`plumb run`), not of the app.
Searching the index built from the seed data still works.

Later launches open straight to the panel, with search ready from the index
of last time. Searching works offline; only setup and crawling need the internet.

If the node cannot start at all, for example because its data folder is not
writable or the disk is full, the app shows an error message with the reason
and the locations of the data folder and the [log](#troubleshooting), then
quits.

## Use Plumb as your browser's search engine

The panel's **Add to Firefox** button opens Firefox on a page with the two
clicks it takes (Firefox lets no page add a search engine by itself). If
Firefox is not installed, the page opens in the default browser.

While the app runs, its search page is at `http://127.0.0.1:7586` in any
browser on the same computer, and the page offers Plumb to the browser as a
search engine:

- **Firefox:** open `http://127.0.0.1:7586`, right-click the address bar and
  choose **Add "Plumb Search"**. To search with it by default, pick it under
  **Settings > Search > Default Search Engine**.
- **Chrome:** once you have opened `http://127.0.0.1:7586`, Chrome lists
  Plumb Search under **Settings > Search engine > Manage search engines and
  site search** as an inactive shortcut. Activate it there, and make it the
  default from its menu if you like.
- **By hand, in any browser:** add a search engine with the address
  `http://127.0.0.1:7586/search?q=%s`.

Searches reach Plumb only while the app is running. If another program has
port 7586 when the app starts, the app uses a free port instead and shows a
message saying so; a search engine set up with 7586 then gets no answer until
that port is free and the app is started again.

## Where data lives

The node keeps its downloads, records and index in the app's data folder. It
grows to a few hundred megabytes (about 1 KB per site).

| System | Data folder |
| --- | --- |
| Linux | `~/.local/share/io.github.sueheir.plumbsearch` (or under `$XDG_DATA_HOME`) |
| macOS | `~/Library/Application Support/io.github.sueheir.plumbsearch` |
| Windows | `%APPDATA%\io.github.sueheir.plumbsearch` (`C:\Users\<you>\AppData\Roaming\...`) |

Inside it, `records.jsonl` holds everything the node has learned, and
`records.jsonl.journal`, when there is one, the crawl results not yet folded
into `records.jsonl`. They are the files to back up, together and with the
app quit, so that they match. `seed/` holds the downloads and `indexes/` the
search index. On Linux the app's log folder, `logs/`, is in there too; see
[Troubleshooting](#troubleshooting).

The window's web view keeps a small cache of its own, in
`~/.cache/io.github.sueheir.plumbsearch/webview` on Linux,
`%LOCALAPPDATA%\io.github.sueheir.plumbsearch\webview` on Windows, and
`~/Library/WebKit/io.github.sueheir.plumbsearch` and
`~/Library/Caches/io.github.sueheir.plumbsearch` on macOS.

To start over, quit the app and delete the data folder; the next launch sets
everything up again. Uninstalling the app leaves these folders in place.

## Troubleshooting

The app writes a log of what it and its node do to `plumb.log` in its log
folder. Each start moves the previous run's log to `plumb.log.1`, replacing
the one before, so when something goes wrong, send us `plumb.log`, and
`plumb.log.1` too if the app has been started again since.

| System | Log folder |
| --- | --- |
| Linux | `~/.local/share/io.github.sueheir.plumbsearch/logs` (or under `$XDG_DATA_HOME`) |
| macOS | `~/Library/Logs/io.github.sueheir.plumbsearch` (in Finder, **Go > Go to Folder**) |
| Windows | `%LOCALAPPDATA%\io.github.sueheir.plumbsearch\logs` (paste it into File Explorer's address bar) |

- **Watch the log as it is written:** start the app from a terminal:
  `plumb-desktop` on Linux, or
  `"/Applications/Plumb Search.app/Contents/MacOS/plumb-desktop"` on macOS.
  Set `RUST_LOG=debug` for more detail, in the terminal and the log file
  alike.
- **Blank or white window on Linux** (seen with some NVIDIA drivers and in
  virtual machines): start the app with `WEBKIT_DISABLE_DMABUF_RENDERER=1
  plumb-desktop`.
- **Quitting:** choose **Quit Plumb Search** from the app's icon in the menu
  bar or notification area (or **Quit** in the macOS app menu). Closing the
  window does not quit. Quitting stops the node at its next safe point. An
  index build under way is finished first, which can take up to about half
  a minute, so the app may keep running briefly. The node's files are never
  left half-written.
- **No icon in the notification area on Linux:** GNOME shows such icons
  only with the AppIndicator extension, which Ubuntu includes. Without it,
  start the app again to bring its window back, and quit with
  `pkill plumb-desktop`, which stops the node cleanly too.

## Build it yourself

The app is the `plumb-desktop` crate in `crates/plumb-desktop`. A plain
`cargo build` or `cargo test` at the repository root leaves it out, since it
needs GUI libraries; build it with `-p plumb-desktop` or with the Tauri CLI.

### Prerequisites

- Rust, from [rustup.rs](https://rustup.rs).
- The Tauri CLI: `cargo install tauri-cli --version "^2" --locked`.
- **Linux:** a C toolchain and the WebKitGTK development files. On Debian and
  Ubuntu:

  ```sh
  sudo apt install build-essential pkg-config libwebkit2gtk-4.1-dev librsvg2-dev file
  ```

  On Fedora, `sudo dnf install webkit2gtk4.1-devel librsvg2-devel` and
  `sudo dnf group install c-development`; on Arch, `sudo pacman -S --needed
  base-devel webkit2gtk-4.1 librsvg`. Tauri's
  [prerequisites page](https://v2.tauri.app/start/prerequisites/) covers
  other distributions.
- **macOS:** the Xcode command line tools: `xcode-select --install`.
- **Windows:** the Microsoft C++ Build Tools with the "Desktop development
  with C++" workload, and WebView2 (already part of Windows 10 and 11). The
  TLS library's crypto code (aws-lc-sys) is assembled with
  [NASM](https://www.nasm.us); without NASM installed, set
  `AWS_LC_SYS_PREBUILT_NASM=1` to use the prebuilt objects it ships with
  (`$env:AWS_LC_SYS_PREBUILT_NASM = "1"` in PowerShell).

### Run and build

From `crates/plumb-desktop`:

```sh
cargo tauri dev     # build and run with the log in the terminal
cargo tauri build   # build the installers into <repository>/target/release/bundle/
```

`cargo tauri build` makes every installer the system supports. To make only
some, list them, for example `cargo tauri build --bundles deb` or
`--bundles nsis`. For a Mac app that runs on both Apple silicon and Intel,
add both Rust targets and build for `universal-apple-darwin`; the installer
lands in `target/universal-apple-darwin/release/bundle/`:

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo tauri build --target universal-apple-darwin
```

Builds downloaded from the internet are the ones macOS and Windows warn
about; an app you build yourself opens without those warnings. To your
system, a local build and an installed copy are the same app: they share the
data folder, and starting one while the other runs only brings the running
one's window to the front. Quit the installed app before `cargo tauri dev`.

On Windows, building the `.msi` runs the WiX toolset, which Tauri downloads
the first time. If that step fails, check that the **VBSCRIPT** optional
feature is enabled in Windows settings, or build only the NSIS installer with
`--bundles nsis`.

### The app icon

The icon is drawn in `crates/plumb-desktop/app-icon.svg`. After changing it,
render it to a 1024x1024 PNG and regenerate the platform icons in `icons/`:

```sh
cd crates/plumb-desktop
cargo run --example render_icon   # writes app-icon.png
cargo tauri icon app-icon.png
rm -r icons/android icons/ios      # mobile icons, not used
```

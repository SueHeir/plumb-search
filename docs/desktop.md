# The Plumb Search desktop app

The desktop app is Plumb Search in a window of its own. It runs a Plumb Search
node inside the app, on your computer, and shows the node's search page. The
same app builds for Windows, macOS and Linux with [Tauri](https://v2.tauri.app).

Search results open in your default browser, not in the app window, and so
does the "As JSON" link under them. Starting the app again while it is
running brings its window to the front.

Test builds are not code-signed yet, so each system warns you before it opens
them the first time. The [install](#install) steps say how to get past that.

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

The window opens with "Starting..." and then shows the node's setup page. On
the first launch the node sets up its index in the background:

1. It downloads the seed data: the Tranco list of popular sites (from
   tranco-list.eu) and the official websites listed in Wikidata (from
   query.wikidata.org). **This needs an internet connection.**
2. It folds them into records for the best-ranked 250,000 sites and builds the
   search index.
3. It crawls the homepages of the top 2,000 sites to fill in titles and
   descriptions, then keeps crawling 1,000 more every 12 hours while the app
   runs.

The setup page shows the progress and switches to the search box when the
first index is ready. If a download fails, for example without internet
access or behind a firewall that blocks those sites, the error appears on the
setup page.

The app has no proxy setting yet. Its downloads go through a proxy only when
one is set in the `HTTPS_PROXY` or `ALL_PROXY` environment variable of the
app (it does not read the system's proxy settings), and it always fetches
homepages directly. So on a network that reaches the internet only through a
proxy, its crawls fail: the search page says that the last update failed,
and `http://127.0.0.1:7586/api/status` suggests `--use-system-proxy`, which
is an option of the command-line node (`plumb run`), not of the app.
Searching the index built from the seed data still works.

Later launches open straight to the search page with the index from last
time. Searching works offline; only setup and crawling need the internet.

If the node cannot start at all, for example because its data folder is not
writable or the disk is full, the app shows an error message with the reason
and the locations of the data folder and the [log](#troubleshooting), then
quits.

## Use Plumb as your browser's search engine

While the app runs, its search page is also at `http://127.0.0.1:7586` in
any browser on the same computer, and the page offers Plumb to the browser
as a search engine:

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
- **Quitting:** closing the window stops the node at its next safe point.
  An index build under way is finished first, which can take up to about
  half a minute, so the app may keep running briefly after its window
  closes. The node's files are never left half-written.

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

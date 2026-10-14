# GitHub plugin

GitHub repositories in your node's results, through GitHub's official API. With a token it also shows which repositories you have starred and gives you Star and Unstar buttons.

| Search | Shows |
| --- | --- |
| `gh http client`, `tokio github` | Up to 8 repositories with their stars, language and last push, most relevant first |
| `gh https://github.com/tokio-rs/tokio` | That repository |
| `python asyncio tutorial`, `pyenv source code` (a search about something whose GitHub account the node knows, or with a word such as `github`, `repository`, `source code` or `open source`) | A "Show results from GitHub" link; following it searches GitHub |
| `rust`, `linus torvalds` (a search that only names such a thing), through that link | The account's most starred repositories |

A search of several words that finds fewer than 3 repositories by their names, descriptions and topics is tried again with READMEs included. Words for GitHub at the end of a search (`repo`, `github`, `source code`) are left out of what GitHub is asked.

With a token, it also:

- marks the repositories you starred "Starred", with a Star or Unstar button on each repository (for you only, on the node's computer);
- puts a star count badge ("GitHub: ★ 12k") and the same buttons on GitHub repositories among the node's own results;
- answers page lookups for repository pages on `github.com`, for a browser extension.

The node reuses a search's results for an hour (`cache_seconds` 3600). To have searches that fit it run GitHub on their own rather than show the link, pick **Run it when a search fits** under **Search & browser → Plugins** in the panel.

## Needs

Nothing, to search. Without a token GitHub allows 10 searches a minute, and a search past that shows nothing from GitHub; the node's log says why.

For the rest, a [fine-grained token](https://github.com/settings/personal-access-tokens) with read and write access to **Starring**, in `config.json` (see [config.example.json](config.example.json)):

```json
{
  "token": "github_pat_..."
}
```

## Install

From the repository root:

```sh
rustup target add wasm32-unknown-unknown
cargo build --release -p plumb-plugin-github --target wasm32-unknown-unknown
```

Make a folder `github` in your node's `plugins/` folder ([Install a plugin](../../docs/plugins.md#install-a-plugin) says where) with:

- `plugin.json` from this folder;
- `plugin.wasm`: `target/wasm32-unknown-unknown/release/plumb_plugin_github.wasm`;
- `config.json` with your token, if you have one.

Restart the node, then search `gh http client`. To try it first:

```sh
plumb try-plugin --plugin path/to/github gh http client
```

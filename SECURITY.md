# Security

## Reporting a problem

Please report security problems privately, not in a public issue. Use GitHub's private vulnerability reporting: [open a security advisory](https://github.com/SueHeir/plumb-search/security/advisories/new).

Say what is affected, how to reproduce it, and what an attacker could do with it. You will get an answer as soon as we can, and a fix is credited to you in the release notes unless you'd rather not be named.

There is no bug bounty.

## Scope

- **The node**: `plumb` and everything it serves (the search page, the JSON API, `/mcp`, private search, the settings panel and remote control), the crawler and plugins' sandbox. The Docker image and the desktop app run the same node.
- **The network**: how nodes connect, sign and check crawls, answer bucket requests, relay sealed requests and hand out tokens.
- **The public server**: plumbsearch.org and its setup in [`site/`](site/).

Out of scope: problems that need control of the computer the node runs on, denial of service by sheer volume, and reports from automated scanners without a working example.

## Supported versions

Fixes go into the latest release and `main`.

# Remote-control transport

The desktop's **Connect to a node** client sends a bearer token that authorizes
changes to the remote node. Connections outside this computer now require HTTPS
with certificate verification. This includes LAN, Docker bridge, private IPv6,
and VPN addresses. An omitted scheme defaults to HTTPS for these addresses.
The client does not follow redirects or use a system proxy.

HTTP is accepted only for loopback IP literals or the exact name `localhost`.
For HTTP, `localhost` is replaced with `127.0.0.1` before connecting, so DNS or a
hosts-file entry cannot redirect a plaintext token to another computer.
Subdomains such as `node.localhost`, the name `localhost.`, and arbitrary names
resolving to loopback do not receive this exception. HTTPS names continue to use
normal name resolution and certificate verification.

Saved connections from earlier versions are checked before every control
request. An insecure saved connection remains available to forget, but cannot
fetch a panel or change settings. The error explains how to reconnect securely.
The client never silently upgrades an explicitly saved HTTP connection or
disables certificate verification.

## A node on your own network: its own certificate

A node started with `--https-bind`, such as

```sh
plumb run --data DIR --bind 0.0.0.0:8080 --https-bind 0.0.0.0:8443
```

also serves HTTPS on port 8443 with a certificate it makes for itself the
first time (`DIR/remote-control-cert.der`, with its key in
`DIR/remote-control-key.der`, readable by its owner only). No certificate
authority signs it, so the desktop pins it instead: `plumb remote-control on`
prints the certificate's SHA-256 fingerprint next to the token, and
`plumb remote-control status` prints it again later (the token it cannot).
In **Connect to a node**, enter `https://<node-address>:8443`, the token and
the fingerprint.

That connection trusts exactly one certificate at that address and checks
nothing else about it: no name, no expiry date, no authority. A different
certificate, such as another machine answering at that address, fails the
handshake before the token is sent. The fingerprint is only accepted for an
HTTPS address, and an HTTPS address without one still needs a certificate
the system trusts.

The certificate is kept across restarts and new tokens. Deleting both files
makes a new one at the next start, after which saved connections stop with
an explanation until they are made again with the new fingerprint. Copy the
fingerprint from the node itself (its terminal or its own panel), not from a
message anyone else could have changed.

The HTTPS port serves the same pages and APIs as the HTTP one, under the
same rules: the control API still takes requests only from local networks
unless public addresses were allowed, and the panel changes settings only
from the node's own computer.

## Reconnecting an existing HTTP node

Start the node with `--https-bind` and connect with its fingerprint (above),
use an HTTPS endpoint with a valid certificate, or keep its existing HTTP
listener behind an SSH tunnel. For example, if the remote node listens on
port 8080:

```sh
ssh -N -L 127.0.0.1:18080:127.0.0.1:8080 user@node-host
```

Then connect in the desktop to `http://127.0.0.1:18080` with the remote node's
token. Keep SSH's host-key verification enabled. The token travels over the
encrypted SSH connection between computers. The remote HTTP hop in this example
also stays on that server's loopback interface.

For a direct HTTPS connection, choose a hostname matching the certificate. A
plain HTTP listener does not gain TLS just because its address is typed with
`https://`; an HTTPS endpoint must already be provided by the operator. Existing
HPC/LAN HTTP control entries must be reconnected using one of these options.

## Scope

This policy protects requests made by Plumb's desktop remote-control client.
The remote control API still accepts HTTP requests from other clients according
to its existing access policy. Operators must provide secure transport and
limit access to that listener. Only `--https-bind` provisions TLS, and only
with the node's own certificate; nothing here deploys nodes or rotates
tokens. Rotate any token previously sent over unencrypted off-host
HTTP or otherwise disclosed. Loopback HTTP assumes the local computer is
trusted; a compromised local process or trusted TLS endpoint can still access
credentials.

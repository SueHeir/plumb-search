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

## Reconnecting an existing HTTP node

Use the node's HTTPS endpoint with a valid certificate, or keep its existing
HTTP listener behind an SSH tunnel. For example, if the remote node listens on
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
limit access to that listener. The change does not provision TLS, deploy nodes,
or rotate tokens. Rotate any token previously sent over unencrypted off-host
HTTP or otherwise disclosed. Loopback HTTP assumes the local computer is
trusted; a compromised local process or trusted TLS endpoint can still access
credentials.

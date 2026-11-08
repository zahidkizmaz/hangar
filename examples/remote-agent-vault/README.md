# Remote Agent Vault

An Agent Vault server that already runs somewhere else, and a bay on this
machine behind it. Point `url` at its proxy port and the CA at its API:

```sh
cp hangar.json ~/.config/hangar/hangar.json   # or merge it into yours
hangar up
```

`up` asks for the agent token: create it on the server's **Agents** page
with the proxy role on the `default` vault.

Agent Vault's proxy port is plain HTTP, so the bay's token crosses the
network unencrypted. That's why the proxy needs `"unsafe": true`, and
why `up` warns every time. Agent Vault's own advice applies: reach it
over a network you trust, such as a VPN or a private subnet.

The bay can reach the server's proxy port and nothing else. hangar
resolves `vault.example.com` at `up` and gives the bay that IP; if the
server moves, the next `up` recreates the bay.

Pin the CA: whoever serves it can read the bays' traffic. Put the output
of this in `sha256`:

```sh
curl -s https://vault.example.com/v1/mitm/ca.pem \
  | openssl x509 -noout -fingerprint -sha256
```

See [proxy-kinds.md](../../docs/proxy-kinds.md#remote-proxies).

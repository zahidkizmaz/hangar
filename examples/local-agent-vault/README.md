# Local Agent Vault

Infisical's open-source Agent Vault, run by hangar in its own VM, and two
bays behind it:

- `vault`, the Agent Vault server: its proxy on `127.0.0.1:14322`, its UI
  on `http://127.0.0.1:14321`, its data in hangar's data folder;
- `work`, which logs in to the `default` vault with the shared token;
- `oss`, which logs in to its own `oss` vault with a token only it has.

```sh
cp hangar.json ~/.config/hangar/hangar.json   # or merge it into yours
hangar up
```

`up` asks for a new master password and starts `vault`. Before `work`
starts it asks for the agent token: open `http://127.0.0.1:14321`,
register (the first account owns the server), add your credentials and
services to the `default` vault, then on the **Agents** page create an
agent with the proxy role on `default` and paste its token. Before `oss`
starts it asks for `token@oss`: create the `oss` vault and a second agent
with the proxy role on `oss` only. Enter skips a token; `hangar up` asks
again next time.

Agent Vault logs every request the bays make through it. See
[proxy-kinds.md](../../docs/proxy-kinds.md#agent-vault).

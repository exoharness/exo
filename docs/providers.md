# Providers

Create a provider profile, log in, and select it:

```sh
exo provider create remote --url http://localhost:8000/exo \
  --context workspace=support
exo provider login remote
exo provider switch remote
```

Run `exo provider` (or `exo provider get`) to see the current selection, its
originating directory or global scope, and context. `exo provider list` also shows
the selection above the saved profiles.

`exo provider clear` clears the global selection. `exo provider clear --local`
clears the override saved in the current directory; parent-directory selections
still apply. Neither command deletes profiles, credentials, or pinned aliases.
Without a saved selection, new commands use built-in local Exo.

`create` and `update` save local configuration. `login` checks the connection and authenticates.

Provider context is a string map sent as JSON in `X-Exo-Context`. Its keys are defined by the provider; Exo forwards these values without interpreting them.

```sh
exo provider update remote --context workspace=support
exo provider switch remote --local --context workspace=development
```

`switch` persists a global selection. `--local` applies to the current directory and its descendants; the nearest directory selection wins. `--provider NAME` overrides the selection for one command and uses that profile's context.

`create` and `update` set the profile's default context. Supplying `--context` replaces the entire map; omitting it on `update` preserves the current map. Use `--context ''` to clear it. Keys and values must be nonempty, and duplicate keys are rejected.

Context supplied to `switch` belongs to that selection and does not modify the profile. Switching without `--context` removes any previous context override for that selection and uses the profile's context. This also applies when repeating `switch --local`. Use `--context ''` to send no context, overriding the profile. Use `provider get NAME` to see both the profile context and the effective context for the current directory.

Saved agent and thread aliases retain their provider, account, endpoint, and context. Changing a default or a profile's context does not move existing aliases. Changing the provider's endpoint or account rejects affected aliases until the original connection is restored.

Runtime options follow the runtime command, for example `exo agent run --root .exo --harness codex --agent-file agent.md`. Provider management commands do not accept harness, sandbox, pricing, or local runtime options. `--scope` on provider create/update selects OAuth scopes.

The managed-agent HTTP API currently supports listing vaults and secret metadata; vault creation/deletion and secret writes require local Exo.

When a provider needs context, it can return HTTP 400 with a suggested context map:

```json
{
  "code": "context_required",
  "message": "Select a workspace.",
  "context": { "workspace": "<workspace>" }
}
```

The CLI fills missing context keys from the suggestion, preserving your existing values. The repair command updates the selected profile, global selection, or originating directory selection. Saved aliases pin their context; changing a selection does not repair an alias. Other HTTP errors retain the server's response and request URL.

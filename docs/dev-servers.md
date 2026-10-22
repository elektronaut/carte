# Dev servers

A Rails app often runs a JavaScript dev server next to Rails, like
vite_ruby's `bin/vite dev` or Shakapacker's webpack-dev-server. Such a
process serves some paths of the app's hostname:

```toml
[apps.shop.processes.vite]
command = "bin/vite dev"
paths = ["/vite-dev/"]

[apps.shop.processes.vite.provides]
VITE_RUBY_PORT = "{{port}}"
```

- It gets a port of its own, like every process. carte sends requests
  under its paths to it, WebSocket upgrades included, and everything
  else to the HTTP process. The browser only talks to the app's
  hostname, so stacks don't collide on a fixed port, and shared apps get
  their assets too.
- A path takes itself and everything below it: `/vite-dev/` takes
  `/vite-dev` and `/vite-dev/@vite/client`, but not `/vite-development`.
  When paths of two processes match, the longer one wins. Two processes
  can't have the same path. Paths match the request's path as it
  arrives, before any decoding of `%` escapes or `..`.
- Its paths go to it ahead of [`serve_files`](projects.md#serving-files), so files
  an earlier build left in `public/vite-dev` aren't served instead.
- [`provides`](processes.md#services-and-tasks) tells Rails and the dev server the
  port, since `{{port}}` there is the dev server's port. It's set even
  while the dev server is down, so Rails never falls back to the
  default port.
- It counts toward the app's readiness like the HTTP process. It takes
  `ready`, `"port"` by default, and the first request waits for both
  under the boot timeout, so the first page doesn't render while its
  assets have nowhere to go. One that exits during the start or isn't
  ready in time fails the start, with its log on the error page.
- While it's disabled or stopped by hand, its paths go to the HTTP
  process. Rails then builds and serves the assets itself.
- It runs per instance, and takes no `kind`, `restart` or
  `scope = "checkout"`. The HTTP process can't have `paths`.

After the start it's always restarted, like the HTTP process; see
[Crashes](processes.md#crashes).

## Presets

carte recognizes the two dev servers by their command, and fills in
`paths` and the port variable. Your own `paths` and `provides` win over
the preset's. Webpacker's older `bin/webpacker-dev-server` and
`bin/webpack-dev-server` aren't recognized.

| Command | `preset` | `paths` | `provides` |
| --- | --- | --- | --- |
| `bin/vite dev`, `bundle exec vite dev` | `"vite_ruby"` | `["/vite-dev/"]` | `VITE_RUBY_PORT = "{{port}}"` |
| `bin/shakapacker-dev-server` | `"shakapacker"` | `["/packs/", "/ws"]` | `SHAKAPACKER_DEV_SERVER_PORT = "{{port}}"` |

Set `preset` for a command carte doesn't recognize, and
`preset = "none"` to keep carte from detecting one. A detected preset
gives way when the process sets `kind`, `restart` or
`scope = "checkout"`, which a process with paths can't have: the process
runs as before, and `carte env` and `carte project edit` show a notice. Remove the setting and
set `preset` to have carte route its paths. Change `paths` when
the app has a different `publicOutputDir` (vite_ruby) or
`public_output_path` (Shakapacker).

Both dev servers read their port from the variable, and so does Rails,
which needs it to tell whether the dev server runs. Hot reloading needs
one change in the app:

- **vite_ruby** points Vite's hot-reload socket at the dev server's port
  on the page's hostname, which doesn't get through carte. Set
  `clientPort` to `0` in `vite.config.ts`: 0 tells Vite to use the port
  the client was loaded from, so the socket goes through carte:

  ```js
  export default defineConfig({
    server: { ws: { clientPort: 0 } }, // Vite 8.0 and older: server.hmr.clientPort
    plugins: [RubyPlugin()],
  })
  ```

  Outside carte, Vite then falls back to connecting to its own port, so
  `bin/dev` keeps working; the browser console shows one failed
  connection before the fallback.
- **Shakapacker** points webpack-dev-server's socket at
  `localhost:3035`. Point it at the page's host and port in
  `config/shakapacker.yml`, and carte passes `/ws` on:

  ```yaml
  development:
    dev_server:
      client:
        webSocketURL: "auto://0.0.0.0:0/ws"
  ```

  Outside carte, Rails doesn't pass the socket on, so hot reloading only
  works under carte with this setting.

vite_ruby records a running dev server in `tmp/vite-ruby-<mode>.json`,
and Rails trusts the record while its port matches. The dev server
removes it when it exits by itself, but not when it's killed, so when a
vite_ruby dev server exits, carte removes the record of its port too.

Vite accepts the app's other hostnames through
`__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS` (see [Node apps](processes.md#node-apps)).
webpack-dev-server refuses any hostname outside `.localhost` with a 403
"Invalid Host header", unless `allowed_hosts` in `config/shakapacker.yml`
lists it: add the project's domain for another TLD, and the share
domain for a shared app.

Settings that make the browser load assets from the dev server directly,
or over TLS, go around carte. Leave them off under carte:

- vite_ruby: `skipProxy: true`, `assetHost` (`VITE_RUBY_ASSET_HOST`) and
  `https: true`
- Vite: `server.ws.host` and `server.ws.protocol`
  (`server.hmr.host` and `server.hmr.protocol` before Vite 8.1)
- Shakapacker: `dev_server.server: https`, and `SHAKAPACKER_ASSET_HOST`

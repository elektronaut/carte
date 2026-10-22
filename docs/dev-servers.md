# Dev servers

A Rails app often runs a JavaScript dev server next to Rails, like
vite_ruby's `bin/vite dev` or Shakapacker's webpack-dev-server. carte
routes some paths of the app's hostname to such a process, and the rest
to Rails. For vite_ruby and Shakapacker, carte sets this up by itself.
See [Presets](#presets).

To set it up by hand, give the process `paths`:

```toml
[apps.shop.processes.vite]
command = "bin/vite dev"
paths = ["/vite-dev/"]

[apps.shop.processes.vite.provides]
VITE_RUBY_PORT = "{{port}}"
```

The dev server gets a port of its own, like every process. carte sends
requests under its paths to it, WebSocket upgrades included. Everything
else goes to the HTTP process. The browser only talks to the app's
hostname, so stacks don't collide on a fixed port, and shared apps get
their assets too.

## How paths match

- **A path takes itself and everything below it.** `/vite-dev/` takes
  `/vite-dev` and `/vite-dev/@vite/client`, but not `/vite-development`.
- **When paths of two processes match, the longer one wins.** Two
  processes can't have the same path.
- **Paths match the request's path as it arrives,** before any decoding
  of `%` escapes or `..`.
- **A dev server's paths go to it ahead of
  [`serve_files`](projects.md#serving-files).** So files an earlier
  build left in `public/vite-dev` aren't served instead.

## How the dev server runs

- **[`provides`](processes.md#services-and-tasks) tells Rails and the
  dev server the port,** since `{{port}}` there is the dev server's
  port. It's set even while the dev server is down, so Rails never falls
  back to the default port.
- **It counts toward the app's readiness,** like the HTTP process. It
  takes `ready`, which is `"port"` by default. The first request waits
  for both, under the [boot timeout](processes.md#boot-timeout), so the
  first page doesn't render while its assets have nowhere to go.
- **One that exits during the start, or isn't ready in time, fails the
  start.** Its log shows on the error page.
- **While it's disabled or stopped by hand,** its paths go to the HTTP
  process. Rails then builds and serves the assets itself.
- **It runs per instance.** It takes no [`kind`](processes.md#services-and-tasks),
  [`restart`](processes.md#crashes) or
  [`scope = "checkout"`](processes.md#scope).
- **The HTTP process can't have `paths`.**

After the start, a dev server is always restarted when it exits, like
the HTTP process. See [Crashes](processes.md#crashes).

## Presets

carte recognizes the two dev servers by their command. It then fills in
`paths` and the port variable. Your own `paths` and `provides` win over
the preset's.

| Command | `preset` | `paths` | `provides` |
| --- | --- | --- | --- |
| `bin/vite dev`, `bundle exec vite dev` | `"vite_ruby"` | `["/vite-dev/"]` | `VITE_RUBY_PORT = "{{port}}"` |
| `bin/shakapacker-dev-server` | `"shakapacker"` | `["/packs/", "/ws"]` | `SHAKAPACKER_DEV_SERVER_PORT = "{{port}}"` |

Webpacker's older `bin/webpacker-dev-server` and `bin/webpack-dev-server`
aren't recognized.

- **Set `preset`** for a command carte doesn't recognize.
- **Set `preset = "none"`** to keep carte from detecting one.
- **Change `paths`** when the app has a different `publicOutputDir`
  (vite_ruby) or `public_output_path` (Shakapacker).

A detected preset gives way when the process sets `kind`, `restart` or
`scope = "checkout"`, which a process with paths can't have. The process
then runs as before, and `carte env` and `carte project edit` show a
notice. To have carte route its paths, remove the setting and set
`preset`.

Both dev servers read their port from the variable. So does Rails, which
needs it to tell whether the dev server runs.

## Hot reloading

Hot reloading needs one change in the app.

**vite_ruby** points Vite's hot-reload socket at the dev server's port
on the page's hostname, which doesn't get through carte. Set
`clientPort` to `0` in `vite.config.ts`. 0 tells Vite to use the port
the client was loaded from, so the socket goes through carte:

```js
export default defineConfig({
  server: { ws: { clientPort: 0 } }, // Vite 8.0 and older: server.hmr.clientPort
  plugins: [RubyPlugin()],
})
```

Outside carte, Vite then falls back to connecting to its own port, so
`bin/dev` keeps working. The browser console shows one failed
connection before the fallback.

**Shakapacker** points webpack-dev-server's socket at `localhost:3035`.
Point it at the page's host and port in `config/shakapacker.yml`, and
carte passes `/ws` on:

```yaml
development:
  dev_server:
    client:
      webSocketURL: "auto://0.0.0.0:0/ws"
```

Outside carte, Rails doesn't pass the socket on. With this setting, hot
reloading only works under carte.

## Other settings

vite_ruby records a running dev server in `tmp/vite-ruby-<mode>.json`,
and Rails trusts the record while its port matches. The dev server
removes the record when it exits by itself, but not when it's killed. So
when a vite_ruby dev server exits, carte removes the record of its port
too.

Vite accepts the app's other hostnames through
`__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS`. See
[Node apps](processes.md#node-apps).

webpack-dev-server refuses any hostname outside `.localhost` with a 403
"Invalid Host header", unless `allowed_hosts` in
`config/shakapacker.yml` lists it. Add the project's domain for another
TLD, and the share domain for a shared app.

Some settings make the browser load assets from the dev server directly,
or over TLS. They go around carte, so leave them off under carte:

- vite_ruby: `skipProxy: true`, `assetHost` (`VITE_RUBY_ASSET_HOST`) and
  `https: true`
- Vite: `server.ws.host` and `server.ws.protocol`
  (`server.hmr.host` and `server.hmr.protocol` before Vite 8.1)
- Shakapacker: `dev_server.server: https`, and `SHAKAPACKER_ASSET_HOST`

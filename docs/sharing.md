# Sharing

Sharing puts chosen apps on the public internet, for a demo or a webhook,
and is always explicit. Only shared apps are reachable; everything else
returns 404.

## Cloudflare Tunnel

Needs a domain on Cloudflare, with a subdomain or whole domain per
machine:

```sh
brew install cloudflared
cloudflared tunnel login
carte setup --share dev.example.com
```

Apps get hostnames like `billing--feature-x--acme.dev.example.com`, and
any number of apps can be shared at once.

## ngrok

Works with ngrok's free plan and its free static domain, one app at a
time:

```sh
brew install ngrok
ngrok config add-authtoken <token>
carte setup --share <your-domain>.ngrok-free.dev
```

## Sharing an app

```sh
carte share billing@feature-x --apps frontend --ttl 2h --auth
carte unshare @feature-x
```

`--apps` shares other apps of the stack along with it, `--auth` adds a
password, `--ttl` ends the share by itself, and `--pin` keeps the apps
running so visitors never wait for a boot. With ngrok, `--replace` ends
the current share instead of refusing. Shares end when the daemon
restarts.

While an app is shared, `CARTE_PUBLIC_URL` holds its public URL, and the
share domain is added to `RAILS_DEVELOPMENT_HOSTS`.

When a shared app is starting, has crashed or can't be reached, visitors
get a plain page with the status and a short message, such as "This app
isn't available right now". The error page with the log and the carte
commands to run only shows at the app's local address.

## What a shared app sees

Requests through a share reach the app from carte on `127.0.0.1`, like
local ones, with these headers:

- `Host` and `X-Forwarded-Host` are the shared hostname, and
  `X-Forwarded-Proto` is `https`.
- `X-Forwarded-For` holds the addresses the tunnel passed on, without
  loopback ones. When none are left, as with a tunnel that sets no
  `X-Forwarded-For`, it is `192.0.2.1`, a placeholder that is never a real
  visitor.
- `Forwarded`, `X-Forwarded-Port`, `X-Forwarded-Prefix`,
  `X-Forwarded-Scheme`, `X-Forwarded-Ssl`, `X-Real-IP` and `Client-IP` are
  removed.
- With `--auth`, carte takes the `Authorization` header for its password
  and removes it, so the app can't use that header itself.

Since the connection comes from loopback, a dev tool that trusts local
requests by their connection address, such as Django Debug Toolbar with
`127.0.0.1` in `INTERNAL_IPS`, treats every visitor as local. Tools that
read `X-Forwarded-For`, such as Rails' web-console, see the visitor.
Turn such tools off, or limit them to addresses other than loopback,
before sharing an app that runs them.

The `[share]` section of the machine config holds the provider and
domain; see [Configuration](configuration.md#share).

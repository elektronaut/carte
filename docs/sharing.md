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

The `[share]` section of the machine config holds the provider and
domain; see [Configuration](configuration.md#share).

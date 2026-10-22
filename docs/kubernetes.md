# Kubernetes

A project file with a `kube` table is a kube project. Its apps are the
Services of a Kubernetes cluster, and its stacks are the cluster's
namespaces. carte forwards a request for a Service to a pod behind it,
the way `kubectl port-forward` would, without anything to start by
hand.

```toml
# ~/.config/carte/projects/prod.toml
kube = { context = "arn:aws:eks:eu-west-1:123456:cluster/production" }
namespaces = ["monitoring", "logging"]

[apps.grafana]
aliases = ["grafana-prod"]

[apps.vlogs]
service_port = "http"
```

| Host | Goes to |
| --- | --- |
| `grafana.monitoring.prod.localhost` | Service `grafana` in namespace `monitoring` |
| `grafana.prod.localhost` | The namespace that has a Service `grafana`. When several do, an error page lists them |
| `grafana-prod.localhost` | An alias of the Service |

With `namespaces` set, `grafana.prod.localhost` skips the namespaces the
cluster doesn't let you read. It fails only when it can read none of
them. When it doesn't find the Service, the page names the namespaces it
couldn't read.

## Adding a cluster

A cluster is only reachable when it has a project file, so nothing about
your kubeconfig is exposed until you add one.

`carte import kube` offers a project for each context of your
kubeconfig, named after its cluster:

- `arn:aws:eks:…:cluster/production` becomes `production`.
- `gke_acme-infra_europe-north1_staging` becomes `staging`.

It asks for a name when that one is taken. `--folder clusters` puts the
files in a folder of their own.

It reads `--kubeconfig`, else the file in `$KUBECONFIG`, else
`~/.kube/config`. A file other than the default is named in the
projects, since the daemon doesn't see your shell's `$KUBECONFIG`.

Kube projects are never reachable through [sharing](sharing.md).

## Settings

| Key | Default | |
| --- | --- | --- |
| `kube.context` | required | The kubeconfig context |
| `kube.kubeconfig` | `$KUBECONFIG`, then `~/.kube/config` | The kubeconfig file, starting with `/` or `~/` |
| `namespaces` | all | The namespaces carte looks in |
| `env_provider` | detected | `mise`, `direnv`, `shell` or `none`. See below |
| `idle_timeout` | machine's | How long a Service stays connected without requests |

`env_provider` sets the env that kubeconfig login plugins, like `aws` or
`gke-gcloud-auth-plugin`, run with. It's detected as for apps, in your
home directory. See [Environment](environment.md).

`[apps.<service>]` tables add settings to a Service the cluster has.
They don't declare one, and every Service can be reached without a
table.

| Key | Default | |
| --- | --- | --- |
| `aliases` | none | Extra hostnames, as for [apps](projects.md#apps) |
| `namespace` | looked up | The namespace `service.project` and the aliases mean. One of `namespaces` when that's set |
| `service_port` | see below | The Service's port, by name or number, for a Service with several |
| `policy` | `"auto"` | `"disabled"` refuses the hostname |

Without `service_port`, carte picks the port in this order:

1. the Service's only port
2. a port named `http` or `web`, or starting with `http-`
3. port 80, 8080 or 443

Otherwise the error page lists the ports.

The keys about apps on disk, like `root`, `include`, `env` and
`processes`, don't apply to a kube project and are errors.

## Forwarding

carte looks up the Service when a request comes in, and picks a ready
pod behind it.

- **Every connection the proxy opens to the pod gets its own
  port-forward stream.** So a forward survives the laptop sleeping, a
  network change and an expired token. The next connection opens a new
  stream.
- **There's no local port to reach a forward by.** Only requests
  through carte's proxy get there.
- **When the pod goes away,** carte picks another ready one.
- **A forward nobody used for `idle_timeout` is closed.** It opens again
  on the next request.
- **Idle connections the proxy keeps** are closed after 30 seconds.

**Login plugins**, like `aws eks get-token`, run with the env the
project's env provider gives in your home directory. So they find the
tools and profiles your shell has. When one fails, the error page shows
its output, with a hint for `aws`, `gcloud` and `kubelogin`.

**A cluster that doesn't answer**, like after the laptop sleeps or a VPN
drops, gets 30 seconds for each call, login and stream it opens. Then
the request gets a 504 page, "The cluster didn't answer". The Service
listing and `carte logs` fail with the same message. Streams already
open, like a followed log or a WebSocket, may stay quiet as long as
they like.

**Ports that speak TLS** get a TLS connection without certificate
checks. Anything else is plain HTTP. A port speaks TLS when:

1. its `appProtocol` is `https` or `wss` (or `kubernetes.io/wss`)
2. else, when it's named `https` or `wss`
3. else, when it's 443 and neither its `appProtocol` nor its name says
   HTTP

Values starting with `https-` or `wss-` count too, in either the
`appProtocol` or the name.

**The `Host` header** is passed on as received. An app whose own URL
setting names its public domain may redirect there.

**Starts, pod changes, failures and idle stops** show in `carte events`.
When the pod side of a forward fails, as when nothing listens on the
port, the error page, `carte status`, `carte ls`, the terminal UI and
`carte events` show what the cluster said. They show it until the pod
answers again.

### Kubeconfig changes

carte fetches the provider env and reads the kubeconfig again when one
of its files changes. Those files are:

- `kube.kubeconfig`, else each file in `KUBECONFIG` of the provider env,
  else `~/.kube/config`
- files the context names by path: `certificate-authority`,
  `client-certificate` or `client-key`

A file counts as changed when its modification time, size or inode
changes.

carte connects again only when what the project's context resolves to
changed. That's the context, its cluster or its user, the contents of
those files, and for a user with a login plugin, the env the plugin runs
with. Switching `current-context` with `kubectx` or
`kubectl config use-context` changes nothing.

Sometimes a kubeconfig is briefly broken, as in the middle of a rewrite.
While it can't be read or parsed, or lacks the context, its cluster or
its user, carte keeps the connection it has. It logs a warning, and
reads the files again on the next request.

It does the same while a file the context names can't be read, or
connecting with it fails, as when a certificate was replaced but its key
not yet. This holds as long as the context, its cluster and its user are
otherwise the same. When they changed and connecting fails, carte
reports the error.

After a kubeconfig change, and after a change to the project file, new
connections use the new settings. Open ones, websockets included, run
until they close. Open connections end only with:

- `carte down`, `carte restart` or `carte stop-all`
- the idle timeout
- setting the Service to `disabled`
- the pod going away
- the daemon stopping

### Requests from other sites

Requests other sites make are refused with a 403, so a web page can't
reach a cluster with your credentials. carte decides whether a request
is another site's like this:

- It looks at the page that made the request, by its `Origin` header.
  When the `Origin` is missing or hidden (`null`), it uses the
  `Referer` instead.
- A page outside carte's TLDs and loopback is another site.
- When neither header names a page, the request counts as another
  site's if its `Origin` is hidden, or if the browser marks it
  cross-site.

Opening a page yourself always works, also by a link from another site.
But another site can't show a Service in a frame, or have the browser
prefetch or prerender it before you click.

An SSO login that returns to the Service by POST is refused, since it's
a cross-site POST. That covers SAML, and OIDC with
`response_mode=form_post`.

## Logs

`carte logs grafana.prod` shows the logs of the pod the forward uses.
That's the pod every one of your requests reaches. When no forward
runs, it shows a ready pod's logs. Reading logs doesn't open a forward.

- Of a pod's containers, carte picks the one serving the Service's port,
  rather than a sidecar.
- `--search` and `--since` look through up to 10,000 lines.
- Following (`-f`) continues on the next pod when this one goes away.
- Logs stream only while a client reads them.
- Reading logs needs the `pods/log` permission, which is separate from
  port-forwarding.

## In the terminal UI

A kube project starts closed in the terminal UI. Opening it asks the
cluster for all its Services, once per opening. carte lists them by
namespace, beside the ones it already knows of. Namespaces where
something runs or failed are open. The others are closed, with a count.

- `/` finds Services in closed namespaces.
- `o` and `y` open and copy a Service's URL.
- `s` starts its forward.

When the cluster doesn't let you list Services across namespaces, carte
lists them per namespace. When it doesn't let you list namespaces
either, it asks you to name the ones you use in `namespaces`.

The project's row shows its context, or why the cluster can't be
reached, like a failed login. A Service's details show the pod it
forwards to, and its log pane shows its pod's logs.

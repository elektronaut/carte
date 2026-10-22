# Kubernetes

A project file with a `kube` table is a kube project: its apps are the
Services of a Kubernetes cluster, and its stacks are the cluster's
namespaces. carte forwards a request for one to a pod behind the Service,
the way `kubectl port-forward` would, without anything to start by hand.

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
| `grafana.prod.localhost` | The namespace that has a Service `grafana`; when several do, an error page lists them |
| `grafana-prod.localhost` | An alias of the Service |

With `namespaces` set, `grafana.prod.localhost` skips the namespaces the
cluster doesn't let you read, and fails only when it reads none of them.
When it doesn't find the Service, the page names the namespaces it
couldn't read.

A cluster is only reachable when it has a project file, so nothing about
your kubeconfig is exposed until you add one. `carte import kube` offers
a project for each context of your kubeconfig, named after its cluster:
`arn:aws:eks:…:cluster/production` becomes `production`, and
`gke_acme-infra_europe-north1_staging` becomes `staging`. It asks for a
name when that one is taken, and `--folder clusters` puts the files in
a folder of their own. It reads `--kubeconfig`, else the file in
`$KUBECONFIG`, else `~/.kube/config`, and names a file other than the
default in the projects, since the daemon doesn't see your shell's
`$KUBECONFIG`. Kube projects are never
reachable through [sharing](sharing.md).

| Key | Default | |
| --- | --- | --- |
| `kube.context` | required | The kubeconfig context |
| `kube.kubeconfig` | `$KUBECONFIG`, then `~/.kube/config` | The kubeconfig file, starting with `/` or `~/` |
| `namespaces` | all | The namespaces carte looks in |
| `env_provider` | detected | `mise`, `direnv`, `shell` or `none`: the env kubeconfig login plugins like `aws` or `gke-gcloud-auth-plugin` run with. Detected as for apps, in your home directory. See [Environment](environment.md) |
| `idle_timeout` | machine's | How long a Service stays connected without requests |

`[apps.<service>]` tables add settings to a Service the cluster has; they
don't declare one. Every Service can be reached without a table.

| Key | Default | |
| --- | --- | --- |
| `aliases` | none | Extra hostnames, as for [apps](projects.md#apps) |
| `namespace` | looked up | The namespace `service.project` and the aliases mean; one of `namespaces` when that's set |
| `service_port` | see below | The Service's port, by name or number, for a Service with several |
| `policy` | `"auto"` | `"disabled"` refuses the hostname |

Without `service_port`, carte takes the Service's only port, else one
named `http`, `web` or starting with `http-`, else port 80, 8080 or 443.
Otherwise the error page lists the ports.

The keys about apps on disk (`root`, `include`, `env`, `processes` and
the like) don't apply to a kube project and are errors.

## Forwarding

carte looks up the Service when a request comes in and picks a ready pod
behind it. Every connection the proxy opens to it gets its own
port-forward stream, so a forward survives the laptop sleeping, a network
change and an expired token: the next connection simply opens a new
stream. There's no local port to reach a forward by; only requests
through carte's proxy get there. When the pod goes away, carte picks
another ready one. A forward nobody used for `idle_timeout` is closed and
opens again on the next request.

- **Kubeconfig changes**: carte fetches the provider env and reads the
  kubeconfig again when one of its files changes: `kube.kubeconfig`,
  else each file in `KUBECONFIG` of the provider env, else
  `~/.kube/config`, or when a file the context names by path changes:
  `certificate-authority`, `client-certificate` or `client-key`. It
  connects again only when what the project's context resolves to
  changed: the context, its cluster or its user, the contents of those
  files, and for a user with a login plugin, the env the plugin runs
  with. Switching `current-context` with `kubectx` or `kubectl config
  use-context` changes nothing. While the kubeconfig can't be read or
  parsed, or lacks the context, its cluster or its user, as in the
  middle of a rewrite, carte keeps the connection it has, logs a
  warning, and reads the files again on the next request. It does the
  same while a file the context names can't be read or connecting with
  it fails, as when a certificate was replaced but its key not yet, as
  long as the context, its cluster and its user are otherwise the same.
  When they changed and connecting fails, carte reports the error. A
  file counts as changed when its modification time, size or inode
  changes. After a change, and after a change to the project file,
  new connections use the new settings while open ones, websockets
  included, run until they close. Open connections end only with `carte
  down`, `carte restart` or `carte stop-all`, the idle timeout, setting
  the Service to `disabled`, the pod going away, or the daemon stopping.
  Idle connections the proxy keeps are closed after 30 seconds.

- **Login plugins**, like `aws eks get-token`, run with the env the
  project's env provider gives in your home directory, so they find the
  tools and profiles your shell has. When one fails, the error page shows its
  output, with a hint for `aws`, `gcloud` and `kubelogin`.
- **A cluster that doesn't answer**, like after the laptop sleeps or a
  VPN drops, gets 30 seconds for each call, login and stream it opens.
  Then the request gets a 504 page, "The cluster didn't answer", and
  the Service listing and `carte logs` fail with the same message.
  Streams already open, like a followed log or a WebSocket, may stay
  quiet as long as they like.
- **Ports that speak TLS** get a TLS connection without certificate
  checks; anything else is plain HTTP. A port speaks TLS when its
  `appProtocol` is `https` or `wss` (or `kubernetes.io/wss`), else when
  it's named `https` or `wss`, else when it's 443 and neither its
  `appProtocol` nor its name says HTTP. Values starting with `https-`
  or `wss-` count too, in either.
- **The `Host` header** is passed on as received. An app whose own URL
  setting names its public domain may redirect there.
- **Requests other sites make** are refused with a 403, so a web page
  can't reach a cluster with your credentials: a request whose `Origin`,
  or `Referer` when the `Origin` is missing or hidden (`null`), is a page
  outside carte's TLDs and loopback, or one with neither whose `Origin`
  is hidden or that the browser marks cross-site.
  Opening a page yourself always works, including by a link from
  another site, but another site can't show a Service in a frame or
  have the browser prefetch or prerender it before you click. An
  SSO login that returns to the Service by POST (SAML, or OIDC with
  `response_mode=form_post`) is refused, since it's a cross-site POST.

Starts, pod changes, failures and idle stops show in `carte events`.
When the pod side of a forward fails, as when nothing listens on the
port, the error page, `carte status`, `carte ls`, the terminal UI and
`carte events` show what the cluster said, until the pod answers again.

`carte logs grafana.prod` shows the logs of the pod the forward uses,
which is the pod every one of your requests reaches, or of a ready pod
when no forward runs; reading logs doesn't open one. Of a pod's
containers it picks the one serving the Service's port, rather than a
sidecar. `--search` and `--since` look through up to 10,000 lines.
Following (`-f`) continues on the next pod when this one goes away. Logs
stream only while a client reads them, and reading them needs the
`pods/log` permission, which is separate from port-forwarding.

In the terminal UI, a kube project starts closed. Opening it asks the
cluster for all its Services, once per opening, and lists them by
namespace beside the ones carte already knows of: namespaces where
something runs or failed are open, the others closed with a count. `/` finds
Services in closed namespaces, `o` and `y` open and copy a Service's
URL, and `s` starts its forward. When the cluster doesn't let you list
Services across namespaces, carte lists them per namespace, and when it
doesn't let you list namespaces either, it asks you to name the ones you
use in `namespaces`.

The project's row shows its context, or why the cluster can't be
reached, like a failed login; a Service's details show the pod it
forwards to, and its log pane shows its pod's logs.

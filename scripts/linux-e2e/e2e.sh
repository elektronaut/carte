#!/bin/bash
# Runs inside a systemd container as root: prepares the machine, then runs
# `carte setup --tld test` as the user dev, checks what it set up, and
# uninstalls it again. Usage: e2e.sh [--browsers]
set -u

browsers=0
[ "${1:-}" = --browsers ] && browsers=1

failed=0
passed=0
step() { printf '\n== %s\n' "$*"; }
pass() { passed=$((passed + 1)); printf 'PASS  %s\n' "$1"; }
fail() { failed=$((failed + 1)); printf 'FAIL  %s\n' "$1"; }
# check <name> <command…>: passes when the command succeeds; shows its output when not.
check() {
  local name=$1 out
  shift
  if out=$("$@" 2>&1); then pass "$name"; else fail "$name"; printf '%s\n' "$out" | sed 's/^/      /' | tail -20; fi
}
# check_not <name> <command…>: passes when the command fails.
check_not() {
  local name=$1 out
  shift
  if out=$("$@" 2>&1); then fail "$name"; printf '%s\n' "$out" | sed 's/^/      /' | tail -20; else pass "$name"; fi
}
as_dev() { su -l dev -c "$*"; }
# Runs carte setup as dev on a terminal, answering yes to its question.
setup_yes() { as_dev "printf 'y\n' | script -qefc 'carte setup $*' /dev/null"; }

distro=$(. /etc/os-release && echo "$ID")
ca=/home/dev/.local/state/carte/ca.pem
firefox=/home/dev/.mozilla/firefox/e2e.default-release
anchor() {
  case $distro in
    fedora) ls /etc/pki/ca-trust/source/anchors/carte-*.pem ;;
    *) ls /usr/local/share/ca-certificates/carte-*.crt ;;
  esac
}
bundle() {
  case $distro in
    fedora) echo /etc/pki/tls/certs/ca-bundle.crt ;;
    *) echo /etc/ssl/certs/ca-certificates.crt ;;
  esac
}
in_bundle() { openssl verify -no-CApath -CAfile "$(bundle)" "$1"; }
dns_file() {
  case $distro in
    fedora) echo /etc/systemd/dns-delegate.d/carte-test.dns-delegate ;;
    *) echo /etc/systemd/resolved.conf.d/carte-test.conf ;;
  esac
}
port_start() { sysctl -n net.ipv4.ip_unprivileged_port_start; }
# answers <url> [curl options]: the app's page comes back.
answers() { [ "$(as_dev "curl -sS --max-time 20 $*")" = "hello from shop" ]; }
# has_carte <nss db dir>: carte's CA is in it.
has_carte() { certutil -L -d "sql:$1" | grep -q '^carte-'; }
units="carte-http.socket carte-https.socket carte.service"
# each_unit <verb> <answer>: `systemctl --user <verb>`, asked one unit at a time,
# gives that answer for each of carte's units.
each_unit() {
  local unit answer all=0
  for unit in $units; do
    answer=$(as_dev "systemctl --user $1 $unit")
    echo "$unit: $answer"
    [ "$answer" = "$2" ] || all=1
  done
  return $all
}
no_unit_active() {
  local unit answer
  for unit in $units; do
    answer=$(as_dev "systemctl --user is-active $unit")
    echo "$unit: $answer"
    [ "$answer" != active ] || return 1
  done
}
# sysctl_files <file…>: `sysctl --system` reads only these files.
sysctl_files() {
  local read
  read=$(sysctl --system 2>&1 | sed -n 's/^\* Applying \(.*\) \.\.\.$/\1/p')
  echo "read: ${read:-nothing}"
  [ "$read" = "$*" ]
}
main_pid() { as_dev 'systemctl --user show -p MainPID --value carte.service'; }

step "Machine"
systemctl --version | head -1
grep PRETTY_NAME /etc/os-release

# Kernel sysctls outside the network namespace belong to the host. boot.sh
# leaves only /proc/sys/net writable; stop if it didn't.
sysrq=$(cat /proc/sys/kernel/sysrq)
if (echo "$sysrq" > /proc/sys/kernel/sysrq) 2>/dev/null; then
  echo "kernel sysctls are writable in this container; stopping before anything changes them"
  exit 1
fi
check "sysctl --system reads no distribution files" sysctl_files

# Docker lets a container's network namespace open any port; a real machine
# doesn't, and setup's sysctl is what's under test.
sysctl -qw net.ipv4.ip_unprivileged_port_start=1024

# Docker bind-mounts its own resolv.conf. Point it at resolved's stub, and give
# resolved the upstream server on the link, as on a desktop.
upstream=$(awk '/^nameserver/ { print $2; exit }' /etc/resolv.conf)
printf 'nameserver 127.0.0.53\noptions edns0 trust-ad\n' > /etc/resolv.conf
link=$(ip -o route show default | awk '{ print $5; exit }')
resolvectl dns "$link" "$upstream"
resolvectl default-route "$link" yes
resolvectl domain "$link" '~.'
resolvectl dns "$link"

loginctl enable-linger dev
for _ in $(seq 30); do
  as_dev 'systemctl --user is-system-running' >/dev/null 2>&1 && break
  sleep 1
done
manager=$(as_dev 'systemctl --user is-system-running' 2>&1)
echo "user manager: $manager"
case $manager in
  running | degraded) ;;
  *)
    systemctl --no-pager status "user@$(id -u dev).service"
    journalctl -b --no-pager | tail -40
    ;;
esac

# A project with one app that carte runs on demand.
as_dev 'mkdir -p ~/shop ~/.config/carte/projects
  echo "hello from shop" > ~/shop/index.html
  echo "web: exec python3 -m http.server \$PORT --bind 127.0.0.1" > ~/shop/Procfile
  echo "root = \"~/shop\"" > ~/.config/carte/projects/shop.toml'

# What setup looks for to trust its CA in browsers: Chromium's config
# directory, and a Firefox profile with its certificate database.
as_dev "mkdir -p ~/.config/chromium $firefox
  printf '[Profile0]\nName=default-release\nIsRelative=1\nPath=e2e.default-release\nDefault=1\n' > ~/.mozilla/firefox/profiles.ini
  certutil -N --empty-password -d sql:$firefox"

step "carte setup --dry-run --tld test"
check "dry run exits 0" as_dev 'carte setup --dry-run --tld test'
as_dev 'carte setup --dry-run --tld test'
check "dry run changes nothing: no units" test ! -e /home/dev/.config/systemd/user/carte.service
check "dry run changes nothing: sysctl" test "$(port_start)" = 1024
check "dry run changes nothing: DNS file" test ! -e "$(dns_file)"
check_not "dry run changes nothing: system anchors" anchor
check_not "dry run changes nothing: ~/.pki/nssdb" has_carte /home/dev/.pki/nssdb
check_not "dry run changes nothing: Firefox profile" has_carte $firefox

step "carte setup --tld test"
check "setup exits 0" setup_yes --tld test
each_unit is-active active >/dev/null || tail -20 /home/dev/.local/state/carte/carte.log

step "Ports and units"
check "units active" each_unit is-active active
check "units enabled" each_unit is-enabled enabled
check "sysctl drop-in" grep -qx 'net.ipv4.ip_unprivileged_port_start=80' /etc/sysctl.d/50-carte.conf
check "sysctl value is 80" test "$(port_start)" = 80
check "sysctl --system reads only carte's file" sysctl_files /etc/sysctl.d/50-carte.conf
check "port 80 open" sh -c "ss -Hltn 'sport = :80' | grep -q ."
check "port 443 open" sh -c "ss -Hltn 'sport = :443' | grep -q ."
check "https://shop.localhost answers from the app" answers https://shop.localhost/ --cacert $ca
check "http://shop.localhost answers from the app" answers http://shop.localhost/

step "CA trust"
cp $ca /root/carte-ca.pem
check "CA in the system anchors" anchor
check "CA in the system bundle" in_bundle /root/carte-ca.pem
check "curl trusts https://shop.localhost (system store)" answers https://shop.localhost/
check "CA in ~/.pki/nssdb (Chrome)" has_carte /home/dev/.pki/nssdb
check "CA in the Firefox profile" has_carte $firefox

step "DNS for .test"
check "DNS file $(dns_file)" test -f "$(dns_file)"
cat "$(dns_file)" 2>/dev/null
check "resolvectl query x.test" sh -c "resolvectl query x.test | grep -q 127.0.0.1"
check "getent ahostsv4 x.test" sh -c "getent ahostsv4 x.test | grep -q 127.0.0.1"
check "other names still resolve" getent hosts example.com
check "https://shop.test answers from the app" answers https://shop.test/ --cacert $ca

step "carte setup --check"
check "check exits 0" as_dev 'carte setup --check'
as_dev 'carte setup --check'
check "a second setup has nothing to do" sh -c "su -l dev -c 'carte setup --dry-run' | grep -q 'Everything is set up already'"

step "carte daemon restart"
before=$(main_pid)
check "restart exits 0" as_dev 'carte daemon restart'
after=$(main_pid)
check "daemon has a new pid ($before -> $after)" test -n "$after" -a "$after" != 0 -a "$after" != "$before"
check "units active after restart" each_unit is-active active
check "https://shop.localhost answers after restart" answers https://shop.localhost/ --cacert $ca

if [ $browsers = 1 ] && [ "$distro" = ubuntu ]; then
  step "Browsers"
  echo "skipped: Ubuntu ships Chromium only as a snap, which doesn't run here"
elif [ $browsers = 1 ]; then
  step "Browsers"
  case $distro in
    fedora) check "install chromium" dnf -y -q install chromium ;;
    *) check "install chromium" sh -c "apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq chromium" ;;
  esac
  chromium=$(command -v chromium chromium-browser | head -1)
  chromium_trusts() {
    local out
    out=$(as_dev "timeout 90 $chromium --headless --no-sandbox --disable-gpu --disable-dev-shm-usage \
      --no-first-run --user-data-dir=\$(mktemp -d) --dump-dom $1" 2>&1)
    grep -q 'hello from shop' <<<"$out" || { printf '%s\n' "$out"; return 1; }
  }
  check "chromium trusts https://shop.localhost (NSS)" chromium_trusts https://shop.localhost/
  check "chromium trusts https://shop.test (NSS)" chromium_trusts https://shop.test/
fi

step "carte setup --uninstall"
check "uninstall exits 0" setup_yes --uninstall
check "unit files gone" sh -c "! ls /home/dev/.config/systemd/user/carte*"
check "no unit links left" sh -c "! ls -d /home/dev/.config/systemd/user/*.wants/carte*"
check "units not active" no_unit_active
check "sysctl drop-in gone" test ! -e /etc/sysctl.d/50-carte.conf
check "sysctl value back to 1024" test "$(port_start)" = 1024
check "sysctl --system reads no files" sysctl_files
check_not "port 80 closed" sh -c "ss -Hltn 'sport = :80' | grep -q ."
check_not "port 443 closed" sh -c "ss -Hltn 'sport = :443' | grep -q ."
check_not "CA gone from the system anchors" anchor
check_not "CA gone from the system bundle" in_bundle /root/carte-ca.pem
check "CA files gone" test ! -e $ca
check_not "CA gone from ~/.pki/nssdb (Chrome)" has_carte /home/dev/.pki/nssdb
check_not "CA gone from the Firefox profile" has_carte $firefox
check "DNS file gone" test ! -e "$(dns_file)"
check_not "x.test no longer resolves to carte" sh -c "resolvectl flush-caches; resolvectl query x.test | grep -q 127.0.0.1"
check "other names still resolve" getent hosts example.com

printf '\n%s: %d passed, %d failed\n' "$distro" "$passed" "$failed"
[ $failed = 0 ]

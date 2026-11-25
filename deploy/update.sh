#!/bin/bash
# Self-update, started by the server through wts-update.service (as user kiosk) after it wrote
# $WTS_DATA/update.env (VERSION, URL, SHA256 of the release package, checked by the server).
# Downloads and checks the package, unpacks it next to the running version, switches
# /opt/walltablet/current, restarts server and kiosk, and switches back if the new version is not healthy
# within 2 minutes. Everything is inside main(): bash reads scripts while running.
main() {
  set -u
  local root=/opt/walltablet data=${WTS_DATA:-/var/lib/walltablet}
  exec >>"$data/update.log" 2>&1
  say() { echo "$(date '+%F %T') $*"; }

  local VERSION='' URL='' SHA256='' key value
  [ -r "$data/update.env" ] || { say "Fehler: $data/update.env fehlt"; exit 1; }
  # plain KEY=value lines, read as data, never sourced
  while IFS='=' read -r key value; do
    case $key in VERSION) VERSION=$value ;; URL) URL=$value ;; SHA256) SHA256=$value ;; esac
  done <"$data/update.env"
  if ! [[ $VERSION =~ ^[0-9][0-9A-Za-z.-]{0,31}$ && $URL =~ ^https?://[^[:space:]]+$ && $SHA256 =~ ^[0-9a-f]{64}$ ]]; then
    say "Fehler: update.env ungültig"; exit 1
  fi

  local old old_ver new=$root/releases/$VERSION kiosk_was_ok=0 tmp had_careful=0
  [ -e "$KIOSK_STATE/careful" ] && had_careful=1
  old=$(readlink -f "$root/current")
  old_ver=$(cat "$old/VERSION" 2>/dev/null || echo "?")
  if [ "$old" = "$new" ]; then say "Schon aktuell (Version $VERSION)"; exit 0; fi
  # a kiosk that is already broken must not block the update that may fix it
  curl -fs -m 3 -o /dev/null http://127.0.0.1:8447/api/health && kiosk_was_ok=1

  say "Lade Version $VERSION: $URL"
  tmp=$(mktemp -d "$root/releases/.new-XXXXXX") || { say "Fehler: mktemp in $root/releases"; exit 1; }
  trap 'rm -rf "$tmp"' EXIT
  curl -fsSL --max-time 900 -o "$tmp/pkg.tar.gz" "$URL" || { say "Fehler: Download fehlgeschlagen"; exit 1; }
  echo "$SHA256  $tmp/pkg.tar.gz" | sha256sum -c --status || { say "Fehler: Prüfsumme stimmt nicht"; exit 1; }
  mkdir "$tmp/x" && tar -xzf "$tmp/pkg.tar.gz" -C "$tmp/x" --strip-components=1 --no-same-owner \
    || { say "Fehler: Paket lässt sich nicht entpacken"; exit 1; }
  local b
  for b in wts-server wts-kiosk wts-camera; do
    [ -x "$tmp/x/bin/$b" ] || { say "Fehler: bin/$b fehlt im Paket"; exit 1; }
  done
  [ "$(cat "$tmp/x/VERSION" 2>/dev/null)" = "$VERSION" ] || { say "Fehler: Paket ist nicht Version $VERSION"; exit 1; }
  rm -rf "$new" && mv "$tmp/x" "$new" || { say "Fehler: $new lässt sich nicht anlegen"; exit 1; }

  say "Update $old_ver -> $VERSION"
  switch_to "$new"
  if healthy "$kiosk_was_ok"; then
    rm -f "$data/update.failed"
    prune "$new" "$old"
    say "Fertig, Version $VERSION"
    exit 0
  fi
  say "Version $VERSION läuft nicht stabil, zurück auf $old_ver"
  echo "$VERSION" >"$data/update.failed" # auto-update skips it until a newer one appears
  # a crash loop of the new kiosk says nothing about the GPU path of the old one
  [ "$had_careful" = 1 ] || rm -f "$KIOSK_STATE/careful"
  switch_to "$old"
  if healthy "$kiosk_was_ok"; then say "Zurückgesetzt auf $old_ver"; else say "Auch $old_ver antwortet nicht, bitte prüfen"; fi
  exit 1
}

# atomic: current always points to a complete release. The kiosk's crash guard starts counting anew, so
# crashes of one release do not put the other into safe mode.
switch_to() {
  ln -sfn "$1" /opt/walltablet/current.new && mv -Tf /opt/walltablet/current.new /opt/walltablet/current
  rm -f "$KIOSK_STATE/starts.json"
  sudo -n /usr/bin/systemctl restart wts-server.service wts-kiosk.service
}

# healthy = 200 three times in a row within 2 minutes: /api/health (server up, kiosk running 20 s and not in
# safe mode) if the kiosk ran fine before the update ($1 = 1), else only /api/status (server up)
healthy() {
  local ok=0 url=http://127.0.0.1:8447/api/status
  [ "$1" = 1 ] && url=http://127.0.0.1:8447/api/health
  for _ in $(seq 40); do
    sleep 3
    if curl -fs -m 3 -o /dev/null "$url"; then
      ok=$((ok + 1)); [ "$ok" -ge 3 ] && return 0
    else
      ok=0
    fi
  done
  return 1
}

# keeps the running release and the one before it (rollback by hand), removes the rest
prune() {
  local d
  for d in /opt/walltablet/releases/* /opt/walltablet/releases/.new-*; do
    [ -d "$d" ] || continue
    [ "$(readlink -f "$d")" = "$(readlink -f "$1")" ] || [ "$(readlink -f "$d")" = "$(readlink -f "$2")" ] || rm -rf "$d"
  done
}

# crash guard of the kiosk (runs as the same user kiosk; systemd sets HOME)
KIOSK_STATE=$HOME/.local/share/walltablet
main "$@"; exit

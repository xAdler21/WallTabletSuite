#!/bin/bash
# Self-update, started by the panel through wts-update.service (as user kiosk).
# Fetches the configured repo/branch, switches to it, restarts server and kiosk and goes back to the
# old commit if the new version is not healthy within 90 s.
# Everything is inside main(): bash reads scripts while running, and git reset replaces this file.
main() {
  set -u
  local dir=/opt/walltablet data=${WTS_DATA:-/var/lib/walltablet}
  local log=$data/update.log
  exec >>"$log" 2>&1
  say() { echo "$(date '+%F %T') $*"; }

  cd "$dir" || { say "Fehler: $dir fehlt"; exit 1; }
  local repo branch
  read -r repo branch < <(python3 -c 'import json,sys; u=json.load(open(sys.argv[1]))["update"]; print(u["repo"], u["branch"])' "$data/config.json") \
    || { say "Fehler: config.json nicht lesbar"; exit 1; }

  local old new old_ver new_ver
  old=$(git rev-parse HEAD)
  old_ver=$(cat VERSION)
  say "Hole $repo ($branch)"
  git fetch --quiet "$repo" "$branch" || { say "Fehler: git fetch fehlgeschlagen"; exit 1; }
  new=$(git rev-parse FETCH_HEAD)
  new_ver=$(git show FETCH_HEAD:VERSION 2>/dev/null || echo "?")
  if [ "$old" = "$new" ]; then say "Schon aktuell (Version $old_ver)"; exit 0; fi

  say "Update Version $old_ver -> $new_ver"
  git reset --quiet --hard "$new" || { say "Fehler: git reset fehlgeschlagen"; exit 1; }
  sudo -n /usr/bin/systemctl restart wts-server.service wts-kiosk.service

  if healthy; then say "Fertig, Version $(cat VERSION)"; exit 0; fi

  say "Version $new_ver läuft nicht stabil, zurück auf $old_ver"
  git reset --quiet --hard "$old"
  sudo -n /usr/bin/systemctl restart wts-server.service wts-kiosk.service
  if healthy; then say "Zurückgesetzt"; else say "Auch die alte Version antwortet nicht, bitte prüfen"; fi
  exit 1
}

# healthy = /api/health answers 200 (server up, kiosk polling) three times in a row within 90 s
healthy() {
  local ok=0
  for _ in $(seq 30); do
    sleep 3
    if curl -fs -m 3 -o /dev/null http://127.0.0.1:8447/api/health; then
      ok=$((ok + 1)); [ "$ok" -ge 3 ] && return 0
    else
      ok=0
    fi
  done
  return 1
}

main "$@"; exit

# WallTabletSuite

**Wandtablet als Kiosk.** Läuft auf einem Lenovo MIIX 520 mit Ubuntu 26.04. Zeigt Web-Panels wie Home Assistant oder die [AcerMiracast](https://github.com/xAdler21/AcerMiracast)-Steuerung als Tabs und streamt die Front-Kamera per ONVIF/RTSP an den UniFi NVR. Eingerichtet wird alles über ein Web-Panel, ohne Login.

Geschrieben in Rust, drei kleine Programme. Bibliotheken kommen aus Ubuntu (GTK 4, WebKitGTK 6, GStreamer).

| Teil | Quelle | Was es tut |
|---|---|---|
| Server | [`crates/server`](crates/server) | Web-Panel und API auf Port **80** (auch 8447 für Plintus), Konfiguration, Sperre, HA-Anmeldung, Updates, ONVIF, startet und überwacht die Kamera |
| Kiosk | [`crates/kiosk`](crates/kiosk) | Vollbild unter [cage](https://github.com/cage-kiosk/cage): Tab-Leiste, ein WebKit-View pro Tab (läuft beim Wechsel weiter), Sperrbild, Drehung, sicherer Modus |
| Kamera | [`crates/camera`](crates/camera) | GStreamer: `rtsp://<ip>:8554/stream` (Haupt-Stream, optional mit Ton), `/sub` (klein), Snapshot, Bild-Regler |
| Gemeinsam | [`crates/common`](crates/common) | Konfiguration und ihre Prüfung, Stream-Größen |
| Panel | [`crates/server/panel.html`](crates/server/panel.html) | Sperre, Drehung, Tabs, HA-Anmeldung und Hell/Dunkel, Kamera mit Vorschau, Update, Logs |

## Installation

Ubuntu 26.04 auf dem Tablet (Server-Variante reicht; ein vorhandener Desktop-Login wird abgeschaltet):

```bash
curl -fsSL https://raw.githubusercontent.com/xAdler21/WallTabletSuite/main/release/walltablet.tar.gz | tar xz
sudo bash walltablet-*/deploy/install.sh
```

Das Skript installiert die Pakete und legt den Benutzer `kiosk` an. Es legt das Programm nach `/opt/walltablet/releases/<version>` (`/opt/walltablet/current` zeigt auf die laufende Version) und richtet drei systemd-Units ein: `wts-server`, `wts-kiosk` (cage auf tty1) und `wts-update`. Am Ende startet das Tablet neu, danach läuft der Kiosk. Web-Panel: `http://<tablet>/`.

**Umstieg von 0.1/0.2 (Python):** genauso wie oben. `install.sh` schiebt den alten git-Klon nach `/opt/walltablet.old-python`. Einstellungen und HA-Anmeldung in `/var/lib/walltablet` bleiben erhalten. Die alte Version kann sich nicht selbst auf 0.3 aktualisieren, dafür einmal per Tastatur (`Strg+Alt+F2`, siehe unten) oder SSH die zwei Zeilen ausführen. **Vorher im alten Panel das automatische Update ausschalten**, sonst versucht 0.2 alle paar Minuten das Update, fällt jedes Mal für 1–2 Minuten aus und setzt sich zurück.

**Raus aus dem Kiosk** (USB- oder Cover-Tastatur): `Strg+Alt+F2` öffnet eine Text-Anmeldung, `Strg+Alt+F1` geht zurück zum Kiosk. Klappt auch, wenn der Bildschirm schwarz ist. Alternativ per SSH, falls `openssh-server` installiert ist.

**Home Assistant:** Im Panel unter *Home-Assistant-Anmeldung* Benutzer und Passwort eintragen (am besten ein eigener HA-Benutzer ohne Zwei-Faktor) und beim HA-Tab den Haken *Home Assistant* setzen. Der Server durchläuft den Login von HA selbst und gibt die Tokens nur an den Kiosk auf dem Tablet. Das Passwort zeigt das Panel nie wieder an. Lehnt HA ab, versucht der Server es erst nach geänderten Zugangsdaten erneut (HA sperrt sonst die IP).

**Hell/Dunkel für HA:** im Panel unter *Anzeige* oder per API `POST /api/config` mit `{"ha": {"theme": "dark"}}` (`light`, `dark`, `auto`). Gilt für Tabs mit Haken *Home Assistant*, auch ohne gespeicherte Anmeldung.

**Hochkant:** Im Panel unter *Anzeige* drehen. Der Kiosk dreht sein Bild selbst (in GTK), Touch dreht automatisch mit, ohne Neustart. Hochkant stehen die Tabs oben in einer Zeile, quer links. Einzige Ausnahme beim Drehen: Aufklapplisten (`<select>`) einer Webseite öffnen sich ungedreht.

**Schrift zu klein:** Im Panel unter *Anzeige* die *Seitengröße* hochsetzen (125–250 %, per API `{"zoom": 150}`). Das vergrößert alle Tabs wie Strg+Plus im Browser, ohne Neustart. Der MIIX 520 hat 1920×1200 auf 12 Zoll, unter cage ohne Skalierung, deshalb wirkt 100 % sehr klein.

## Wenn der Kiosk abstürzt

Startet der Kiosk viermal innerhalb von drei Minuten, geht er in den **sicheren Modus**: keine Webseiten, nur ein Hinweis auf dem Bildschirm, das Panel zeigt „sicherer Modus“. Nach 5 Minuten versucht er es wieder, ab dann mit Software-Grafik (die häufigste Absturzursache ist der GPU-Pfad von WebKit). `install.sh` setzt das zurück.

Die Ursache steht im Panel unter **Logs** (Kiosk, Server und Kamera, Update), also im systemd-Journal seit dem letzten Start. Von Hand: `journalctl -u wts-kiosk -b`.

## Sperre

Gesperrt zeigt der Kiosk ein Sperrbild mit Uhr und nimmt keine Eingaben an. Entsperren geht nur über Panel oder API. Der Zustand übersteht Neustarts.

| Aufruf | Wirkung |
|---|---|
| `GET /api/lock` | sperren |
| `GET /api/unlock` | entsperren |
| `POST /api/lock` mit `{"locked": true\|false}` | sperren / entsperren ([Plintus-Vertrag](https://github.com/xAdler21/Plintus-Panel/blob/main/docs/WALL-TABLETS.md)) |
| `GET /api/status` | `version`, `locked`, `devices` (Tablet und Kamera) |

Beispiel für Home Assistant:

```yaml
rest_command:
  wandtablet_sperren:
    url: http://wandtablet/api/lock
  wandtablet_entsperren:
    url: http://wandtablet/api/unlock
```

In **Plintus** unter Einstellungen → Wall-Tablet-Suite die Adresse des Tablets eintragen. Dann erscheint es in der Dienst-Matrix und lässt sich in der App sperren.

## Kamera → UniFi Protect

1. Im Panel *Kamera streamen* einschalten, Kamera wählen (MIIX: Front und Rück), drehen, Auflösung, Bitrate, Keyframe-Abstand und Profil setzen, speichern. Fehler stehen im Log darunter, das Vorschaubild zeigt, ob Bilder kommen.
2. Protect → *Drittanbieter-Kameras* (ONVIF): Das Tablet erscheint per Discovery oder lässt sich per IP hinzufügen. Benutzer und Passwort sind egal.

Was Protect bekommt: Profil `main` (Haupt-Stream, mit AAC-Ton wenn eingeschaltet), Profil `sub` (kleiner Stream für die Live-Ansicht), Snapshot-URL für Vorschaubilder.

**Protect merkt sich die Stream-Adressen beim Hinzufügen.** Dem Tablet per DHCP eine feste IP geben. Nach Änderungen an Sub-Stream, Ton, Auflösung oder IP die Kamera in Protect entfernen und neu hinzufügen.

**Bild-Regler:** Das Panel listet die Regler, die libcamera für die gewählte Kamera meldet (Belichtung, Weißabgleich …), mit ihrem echten Bereich aus `cam --list-controls`. Sie wirken sofort, ohne Neustart. *↺* nimmt den Regler heraus, die Kamera startet dann kurz neu und nutzt wieder den Wert des Treibers.

**Ton:** Fehlt das Mikrofon oder lässt es sich nicht öffnen, läuft der Haupt-Stream ohne Ton weiter. Klappt `default` nicht, `plughw:0,0` probieren (Liste: `arecord -l`).

Die Front-Kamera des MIIX 520 hängt an Intel IPU3, nur **libcamera** liest sie (`cam -l` zeigt, ob sie erkannt wird). Findet libcamera keine Kamera, zeigt der Knopf *Kamera-Diagnose* im Panel, woran es liegt: Video-Geräte, Sensoren laut Firmware, Kernel-Module und -Meldungen, welche libcamera geladen wird. Klappt das nicht, gibt es zwei Auswege: eine USB-Kamera über *V4L2* oder eine *eigene GStreamer-Quelle*. Fehlt der Hardware-Encoder (`vah264enc`), nimmt die Kamera von selbst *x264*.

## Updates

Repo-Link und Branch im Panel. *Nach Updates suchen* liest `release/latest.json` aus dem Repo (Version, Paket, SHA-256) und vergleicht die Version mit der installierten. *Installieren* spielt eine höhere Version ein. Optional prüft und installiert der Server alle 6 h selbst.

[`deploy/update.sh`](deploy/update.sh) lädt das Paket, prüft die Prüfsumme, entpackt es nach `/opt/walltablet/releases/<version>` und schaltet `current` um. Danach startet es Server und Kiosk neu. Antwortet `/api/health` (Server läuft, Kiosk läuft seit 20 s und nicht im sicheren Modus) nicht innerhalb von 2 Minuten dreimal hintereinander, schaltet es auf die alte Version zurück. Eine so zurückgenommene Version installiert die Automatik nicht noch einmal. Log im Panel.

Geänderte systemd-Units oder neue Pakete kommen nicht per Update, dafür die zwei Zeilen aus *Installation* noch einmal ausführen (in einem eigenen Ordner, nicht in `/opt/walltablet`: dort darf der Benutzer `kiosk` schreiben, `install.sh` läuft aber als root).

## Sicherheit

Panel und API haben keinen Login und laufen nur über HTTP. Das ist so gewollt, weil alles im lokalen Netz läuft. Wer das Panel erreicht, kann die Kamera sehen, das Tablet entsperren und über den Repo-Link fremden Code als Benutzer `kiosk` einspielen. Port 8447/80/8554 also nicht ins Internet oder Gäste-WLAN lassen.

## Entwicklung

```bash
cargo test                                  # Server und Konfiguration, läuft auch unter Windows
WTS_DATA=./data cargo run -p wts-server     # Panel auf http://localhost:8447
```

Kiosk und Kamera brauchen Linux (WSL reicht) mit den Entwicklerpaketen aus [`deploy/build-release.sh`](deploy/build-release.sh):

```bash
cargo build --release --workspace
python3 tests/e2e_linux.py                  # alles echt: cage headless, Screenshots, Taps, RTSP, ONVIF (Pakete: grim ffmpeg)
```

**Release:** `VERSION` erhöhen, `bash deploy/build-release.sh` (baut, testet, schreibt `release/walltablet.tar.gz` und `release/latest.json`), beides committen und pushen. Die Tablets finden die neue Version dann über `latest.json`.

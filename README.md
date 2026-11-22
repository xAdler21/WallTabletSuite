# WallTabletSuite

**Wandtablet als Kiosk.** Läuft auf einem Lenovo MIIX 520 mit Ubuntu 26.04. Zeigt Web-Panels wie Home Assistant oder die [AcerMiracast](https://github.com/xAdler21/AcerMiracast)-Steuerung als Tabs und streamt die Front-Kamera per ONVIF/RTSP an den UniFi NVR. Eingerichtet wird alles über ein Web-Panel, ohne Login.

| Teil | Datei | Was es tut |
|---|---|---|
| Server | [`app/server.py`](app/server.py) | Web-Panel und API auf Port **80** (auch 8447 für Plintus), Konfiguration, Sperre, Updates, startet die Kamera |
| Kiosk | [`app/kiosk.py`](app/kiosk.py) | Vollbild unter [cage](https://github.com/cage-kiosk/cage): Tab-Leiste links, ein WebKit-View pro Tab (läuft beim Wechsel weiter), Sperrbild |
| Kamera | [`app/camera.py`](app/camera.py) | GStreamer-RTSP-Server `rtsp://<ip>:8554/stream`, H.264 per VA-API oder x264 |
| ONVIF | [`app/onvif.py`](app/onvif.py) | WS-Discovery und die Device/Media-Aufrufe, mit denen der NVR die Kamera findet |
| Panel | [`app/panel.html`](app/panel.html) | Tabs, Kamera, Sperre, Update |

Nur Python 3 und Pakete aus Ubuntu (GTK 4, WebKitGTK 6, GStreamer), kein Build-Schritt.

## Installation

Ubuntu 26.04 auf dem Tablet (Server-Variante reicht; ein vorhandener Desktop-Login wird abgeschaltet):

```bash
sudo git clone https://github.com/xAdler21/WallTabletSuite.git /opt/walltablet
sudo bash /opt/walltablet/deploy/install.sh
```

Das Skript installiert die Pakete und legt den Benutzer `kiosk` an. Es richtet drei systemd-Units ein: `wts-server`, `wts-kiosk` (cage auf tty1) und `wts-update`. Am Ende startet das Tablet neu, danach läuft der Kiosk. Web-Panel: `http://<tablet>/`.

**Home Assistant:** Im Panel unter *Home-Assistant-Anmeldung* Benutzer und Passwort eintragen (am besten ein eigener HA-Benutzer ohne Zwei-Faktor) und beim HA-Tab den Haken *HA-Login* setzen. Der Server durchläuft den Login von HA selbst und gibt die Tokens nur an den Kiosk auf dem Tablet. Das Passwort zeigt das Panel nie wieder an. Lehnt HA ab, versucht der Server es erst nach geänderten Zugangsdaten erneut (HA sperrt sonst die IP).

**Raus aus dem Kiosk** (USB- oder Cover-Tastatur): `Strg+Alt+F2` öffnet eine Text-Anmeldung, `Strg+Alt+F1` geht zurück zum Kiosk. Klappt auch, wenn der Bildschirm schwarz ist. Alternativ per SSH, falls `openssh-server` installiert ist.

Neue Version von Hand einspielen (z. B. wenn das Panel nicht erreichbar ist):

```bash
sudo git -c safe.directory=/opt/walltablet -C /opt/walltablet pull
sudo bash /opt/walltablet/deploy/install.sh
```

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

1. Im Panel *Kamera streamen* einschalten und speichern. Beim Start testet der Kamera-Prozess die Pipeline, Fehler stehen im Log darunter.
2. Protect → *Drittanbieter-Kameras* (ONVIF): Das Tablet erscheint per Discovery oder lässt sich per IP hinzufügen. Benutzer und Passwort sind egal.

Die Front-Kamera des MIIX 520 hängt an Intel IPU3, nur **libcamera** liest sie (`cam -l` zeigt, ob sie erkannt wird). Klappt das nicht, gibt es zwei Auswege: eine USB-Kamera über *V4L2* oder eine *eigene GStreamer-Quelle*. Ruckelt der Hardware-Encoder oder fehlt er, auf *x264* umstellen.

## Updates

Wie bei Plintus: Repo-Link und Branch im Panel. *Nach Updates suchen* vergleicht die Datei `VERSION` im Repo mit der installierten, *Installieren* spielt eine höhere Version ein. Optional prüft und installiert der Server alle 6 h selbst. Für ein Release also `VERSION` erhöhen (z. B. `0.1.1` → `0.1.2`); Commits ohne neue Version werden nicht angeboten.

[`deploy/update.sh`](deploy/update.sh) holt den Branch per `git fetch`, setzt `/opt/walltablet` darauf und startet Server und Kiosk neu. Antwortet `/api/health` (Server läuft, Kiosk fragt an) nicht innerhalb von 90 s dreimal hintereinander, geht es zurück auf den alten Commit. Log im Panel.

Geänderte systemd-Units oder neue Pakete kommen nicht per Update, dafür `install.sh` erneut ausführen.

## Sicherheit

Panel und API haben keinen Login und laufen nur über HTTP. Das ist so gewollt, weil alles im lokalen Netz läuft. Wer das Panel erreicht, kann die Kamera sehen, das Tablet entsperren und über den Repo-Link fremden Code als Benutzer `kiosk` einspielen. Port 8447/80/8554 also nicht ins Internet oder Gäste-WLAN lassen.

## Entwicklung

```bash
python -m unittest discover tests      # Server, Sperre, Validierung, ONVIF – ohne GTK/GStreamer
WTS_DATA=./data python app/server.py   # Panel auf http://localhost:8447
```

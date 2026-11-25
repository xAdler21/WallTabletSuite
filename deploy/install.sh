#!/bin/bash
# Setup on the tablet (Ubuntu 26.04), run as root from an unpacked release package:
#   curl -fsSL https://raw.githubusercontent.com/xAdler21/WallTabletSuite/main/release/walltablet.tar.gz | tar xz
#   sudo bash walltablet-*/deploy/install.sh
# Also moves an old Python version (git clone in /opt/walltablet, 0.1-0.2) aside; settings in
# /var/lib/walltablet stay. Safe to run again, always from a freshly unpacked package: everything under
# /opt/walltablet belongs to user kiosk, so running a script from there as root would hand root to kiosk.
set -euo pipefail
ROOT=/opt/walltablet
USER_NAME=kiosk
SRC=$(cd "$(dirname "$0")/.." && pwd -P)

[ "$(id -u)" = 0 ] || { echo "Bitte mit sudo ausführen"; exit 1; }
[ -x "$SRC/bin/wts-server" ] && [ -f "$SRC/VERSION" ] || { echo "$SRC ist kein entpacktes Release-Paket (bin/wts-server fehlt)"; exit 1; }
VER=$(cat "$SRC/VERSION")
case "$SRC/" in "$ROOT"/*) echo "Bitte aus einem frisch entpackten Paket ausführen, nicht aus $ROOT (siehe README)"; exit 1 ;; esac

echo "== Pakete"
apt-get update
apt-get install -y --no-install-recommends \
  curl ca-certificates cage fonts-noto-color-emoji libgtk-4-1 libwebkitgtk-6.0-4 \
  gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
  gstreamer1.0-plugins-ugly gstreamer1.0-libav gstreamer1.0-alsa gstreamer1.0-libcamera \
  libgstrtspserver-1.0-0 libcamera-tools libcamera-ipa alsa-utils
# H.264 encoding in hardware on Kaby Lake; the non-free driver (multiverse) has the encoder kernels.
# Without it the camera falls back to x264 on its own.
apt-get install -y intel-media-va-driver-non-free || apt-get install -y intel-media-va-driver || true

echo "== Benutzer $USER_NAME"
id "$USER_NAME" >/dev/null 2>&1 || useradd -m -s /bin/bash "$USER_NAME"
# systemd-journal: the panel shows the logs of kiosk, server and update
usermod -aG video,render,input,audio,systemd-journal "$USER_NAME"
# the user's PipeWire (microphone behind ALSA "default") runs without a kiosk session, and logind does not
# delete the camera snapshot in /dev/shm when the kiosk session ends
loginctl enable-linger "$USER_NAME"

echo "== Version $VER nach $ROOT"
if [ -d "$ROOT/.git" ]; then
  echo "Alte Python-Version gefunden, verschoben nach $ROOT.old-python"
  systemctl stop wts-kiosk.service wts-server.service 2>/dev/null || true
  rm -rf "$ROOT.old-python"
  mv "$ROOT" "$ROOT.old-python"
fi
mkdir -p "$ROOT/releases"
rm -rf "$ROOT/releases/$VER"
cp -a "$SRC" "$ROOT/releases/$VER"
ln -sfn "$ROOT/releases/$VER" "$ROOT/current.new"
mv -Tf "$ROOT/current.new" "$ROOT/current"
chown -R "$USER_NAME:" "$ROOT"
# the 0.2.0 touch rule never worked in cage; the kiosk now turns inside GTK and touch follows by itself
if [ -f /etc/udev/rules.d/90-walltablet-touch.rules ]; then
  rm -f /etc/udev/rules.d/90-walltablet-touch.rules
  udevadm control --reload || true
fi
# fresh start for the kiosk's crash guard (safe mode, software rendering)
home=$(getent passwd "$USER_NAME" | cut -d: -f6)
rm -f "$home/.local/share/walltablet/starts.json" "$home/.local/share/walltablet/careful"

echo "== systemd"
install -m 644 "$SRC/deploy/"wts-*.service /etc/systemd/system/ # from the root-owned package, not kiosk's copy
cat >/etc/pam.d/wts-kiosk <<'EOF'
auth     required pam_unix.so nullok
account  required pam_unix.so
session  required pam_unix.so
session  required pam_systemd.so
EOF
cat >/etc/sudoers.d/walltablet <<EOF
# WallTabletSuite: start the self-update and restart its own services, nothing else
$USER_NAME ALL=(root) NOPASSWD: /usr/bin/systemctl start --no-block wts-update.service, /usr/bin/systemctl restart wts-server.service wts-kiosk.service
EOF
chmod 440 /etc/sudoers.d/walltablet
visudo -cf /etc/sudoers.d/walltablet

systemctl daemon-reload
systemctl enable wts-server.service wts-kiosk.service
systemctl restart wts-server.service
# the kiosk owns tty1, a desktop login manager would fight over the screen. Disabled without --now:
# stopping it would also end the desktop terminal this script may run in, before the reboot below
for dm in gdm3 gdm sddm lightdm; do systemctl disable "$dm" 2>/dev/null || true; done
systemctl set-default graphical.target

ip=$(hostname -I | awk '{print $1}')
echo
echo "Fertig, Version $VER. Web-Panel: http://$ip/"
echo "Kamera: im Panel einschalten, dann in UniFi Protect als ONVIF-Kamera hinzufügen."
[ "${WTS_NO_REBOOT:-}" = 1 ] && exit 0 # tests
echo "Neustart in 10 s, danach läuft der Kiosk auf dem Bildschirm (Strg+C bricht den Neustart ab)."
sleep 10
systemctl reboot

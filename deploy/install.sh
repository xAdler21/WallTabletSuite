#!/bin/bash
# One-time setup on the tablet (Ubuntu 26.04). Run as root from the clone in /opt/walltablet:
#   sudo git clone https://github.com/xAdler21/WallTabletSuite.git /opt/walltablet
#   sudo bash /opt/walltablet/deploy/install.sh
# Safe to run again (e.g. after an update that changed the systemd units).
set -euo pipefail
DIR=/opt/walltablet
USER_NAME=kiosk

[ "$(id -u)" = 0 ] || { echo "Bitte mit sudo ausführen"; exit 1; }
[ -f "$DIR/app/server.py" ] || { echo "Repo muss in $DIR liegen"; exit 1; }

echo "== Pakete"
apt-get update
apt-get install -y --no-install-recommends \
  git curl python3 python3-gi gir1.2-gtk-4.0 gir1.2-webkit-6.0 cage fonts-noto-color-emoji \
  gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad \
  gstreamer1.0-plugins-ugly gstreamer1.0-libcamera gir1.2-gst-rtsp-server-1.0 libcamera-tools
# H.264 encoding in hardware on Kaby Lake; the non-free driver (multiverse) has the encoder kernels
apt-get install -y intel-media-va-driver-non-free || apt-get install -y intel-media-va-driver || true

echo "== Benutzer $USER_NAME"
id "$USER_NAME" >/dev/null 2>&1 || useradd -m -s /bin/bash "$USER_NAME"
usermod -aG video,render,input "$USER_NAME"
chown -R "$USER_NAME:" "$DIR"

echo "== systemd"
install -m 644 "$DIR"/deploy/wts-*.service /etc/systemd/system/
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
echo "Fertig. Web-Panel: http://$ip/"
echo "Kamera: im Panel einschalten, dann in UniFi Protect als ONVIF-Kamera hinzufügen."
echo "Neustart in 10 s, danach läuft der Kiosk auf dem Bildschirm (Strg+C bricht den Neustart ab)."
sleep 10
systemctl reboot

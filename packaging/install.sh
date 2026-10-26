#!/bin/sh
# Installs a built showmesh-ndi binary as a systemd service. Run from the repository root as root.
set -eu

BIN=${BIN:-target/release/showmesh-ndi}
[ -x "$BIN" ] || { echo "No binary at $BIN. Run cargo build --release first."; exit 1; }

id showmesh-ndi >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin \
  --groups video,render showmesh-ndi
install -m 0755 "$BIN" /usr/local/bin/showmesh-ndi
install -m 0644 packaging/showmesh-ndi.service /etc/systemd/system/showmesh-ndi.service
install -d -m 0750 -o showmesh-ndi -g showmesh-ndi /etc/showmesh-ndi
[ -f /etc/showmesh-ndi/config.toml ] || \
  install -m 0640 -o showmesh-ndi -g showmesh-ndi packaging/config.example.toml /etc/showmesh-ndi/config.toml

if [ ! -e /usr/local/lib/libndi.so.6 ] && ! ldconfig -p | grep -q "libndi.so.6 "; then
  echo "The NDI runtime (libndi.so.6) is not installed. See README.md, then start the service."
fi

systemctl daemon-reload
systemctl enable showmesh-ndi
echo "Installed. Edit /etc/showmesh-ndi/config.toml, then: systemctl restart showmesh-ndi"

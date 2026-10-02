set -eu
data="${OCTOPAGE_DATA:-/data}"
mkdir -p "$data"
if [ "$(id -u)" = 0 ]; then
  chown -R octopage:octopage "$data"
  exec setpriv --reuid=octopage --regid=octopage --init-groups /usr/local/bin/octopage-server "$@"
fi
exec /usr/local/bin/octopage-server "$@"

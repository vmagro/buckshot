#!/bin/sh
# Fills in config.json5.tmpl's __CPU__ placeholder with this container's own
# native arch, using buck2's cpu label (see platforms/BUCK, platforms/exec/
# BUCK) rather than Debian's/uname's, so it matches exactly what buck2's
# `local-linux-worker` execution platform sends as its `remote_execution_properties`
# CPU value.
set -eu

case "$(uname -m)" in
    x86_64) cpu=x86_64 ;;
    aarch64) cpu=arm64 ;;
    *) echo "unsupported uname -m: $(uname -m)" >&2; exit 1 ;;
esac

sed "s/__CPU__/$cpu/" /config/config.json5.tmpl > /config/config.json5

exec nativelink /config/config.json5

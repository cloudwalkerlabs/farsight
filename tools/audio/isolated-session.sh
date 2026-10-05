#!/bin/bash
# Starts a private audio session the way farsight-server would (docs/research/audio.md §1):
# a private runtime dir, a private D-Bus, PipeWire + WirePlumber with every hardware monitor
# off, and pipewire-pulse. It never touches /run/user/$UID.
#
#   tools/audio/isolated-session.sh [runtime-dir]     # default: /tmp/fsa-$UID
#   XDG_RUNTIME_DIR=/tmp/fsa-$UID pw-cli ls Node       # inspect it
#   kill $(cat /tmp/fsa-$UID/*.pid)                    # stop it
#
# Keep the runtime dir short: socket paths are limited to 108 bytes.
set -eu
R=${1:-/tmp/fsa-$(id -u)}
C=$R/config
ST=$R/state
rm -rf "$R"
mkdir -p "$R" "$C/pipewire/pipewire.conf.d" "$C/wireplumber/wireplumber.conf.d" "$ST"
chmod 700 "$R"

cat > "$C/pipewire/pipewire.conf.d/farsight.conf" <<'EOF'
context.properties = {
    default.clock.rate          = 48000
    default.clock.allowed-rates = [ 48000 ]
    # One 5 ms Opus frame per graph cycle.
    default.clock.quantum       = 240
    default.clock.min-quantum   = 240
    default.clock.max-quantum   = 240
}
EOF
cat > "$C/wireplumber/wireplumber.conf.d/farsight.conf" <<'EOF'
wireplumber.profiles = {
  farsight = {
    # systemwide-session: no logind, no device reservation, no portal store.
    inherits = [ main, mixin.systemwide-session ]
    hardware.audio = disabled
    hardware.bluetooth = disabled
    hardware.video-capture = disabled
  }
}
EOF

# Built from scratch, as the server builds its children's environment.
ENVV=(env -i PATH=/usr/bin HOME="$HOME" USER="$USER" XDG_RUNTIME_DIR="$R"
      XDG_CONFIG_HOME="$C" XDG_STATE_HOME="$ST" PULSE_SERVER="unix:$R/pulse/native")
"${ENVV[@]}" dbus-daemon --session --address="unix:path=$R/bus" --fork --print-pid > "$R/dbus.pid"
ENVV+=(DBUS_SESSION_BUS_ADDRESS="unix:path=$R/bus")
"${ENVV[@]}" pipewire > "$R/pipewire.log" 2>&1 & echo $! > "$R/pipewire.pid"
sleep 0.5
"${ENVV[@]}" wireplumber --profile farsight > "$R/wireplumber.log" 2>&1 & echo $! > "$R/wireplumber.pid"
"${ENVV[@]}" pipewire-pulse > "$R/pipewire-pulse.log" 2>&1 & echo $! > "$R/pipewire-pulse.pid"
sleep 1.5
XDG_RUNTIME_DIR=$R pw-cli ls Node | grep -E 'node.name|media.class'
echo "devices: $(XDG_RUNTIME_DIR=$R pw-cli ls Device | grep -c device.api || true)"

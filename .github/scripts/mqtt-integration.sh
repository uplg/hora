#!/bin/sh
# Run the MQTT monitors against a real broker: a throwaway Mosquitto 2
# container bound to a free loopback port, stopped and started by the test
# itself. Usage: mqtt-integration.sh
set -eu
here=$(cd "$(dirname "$0")/../.." && pwd)
name=hora-mqtt-$$
cleanup() { docker rm -f "$name" >/dev/null 2>&1 || true; }
trap cleanup EXIT

port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
docker run -d --name "$name" -p "127.0.0.1:$port:1883" \
  -v "$here/.github/scripts/mosquitto.conf:/mosquitto/config/mosquitto.conf:ro" \
  eclipse-mosquitto:2 >/dev/null

HORA_TEST_MQTT_URL="mqtt://127.0.0.1:$port" HORA_TEST_MQTT_CONTAINER="$name" \
  cargo test -p hora-core --locked --test mqtt_mosquitto -- --ignored --nocapture

#!/bin/bash
set -e
cd "$(dirname "$0")/.."

# Load configuration
source configs/backend.env

echo "Starting Signal Calling Backend..."
echo "Binding IP: $BINDING_IP"
echo "Public/ICE IP: $ICE_CANDIDATE_IP"

# Check if running in development (cargo target dir exists) or deployment (bin dir)
if [ -f "./target/release/calling_backend" ]; then
    BINARY="./target/release/calling_backend"
elif [ -f "./bin/calling_backend" ]; then
    BINARY="./bin/calling_backend"
else
    echo "Error: calling_backend binary not found in ./target/release/ or ./bin/"
    exit 1
fi

exec "$BINARY" \
  --binding-ip "$BINDING_IP" \
  --ice-candidate-ip "$ICE_CANDIDATE_IP" \
  --ice-candidate-port "$ICE_CANDIDATE_PORT" \
  --ice-candidate-port-tcp "$ICE_CANDIDATE_PORT_TCP" \
  --signaling-ip "$SIGNALING_IP" \
  --signaling-port "$SIGNALING_PORT" \
  --max-clients-per-call "$MAX_CLIENTS_PER_CALL"

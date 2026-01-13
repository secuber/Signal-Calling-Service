#!/bin/bash
set -e
cd "$(dirname "$0")/.."

# Load configuration
source configs/backend.env

echo "Starting Signal Calling Backend..."
echo "Binding IP: $BINDING_IP"
echo "Public/ICE IP: $ICE_CANDIDATE_IP"

./bin/calling_backend \
  --binding-ip "$BINDING_IP" \
  --ice-candidate-ip "$ICE_CANDIDATE_IP" \
  --ice-candidate-port "$ICE_CANDIDATE_PORT" \
  --ice-candidate-port-tcp "$ICE_CANDIDATE_PORT_TCP" \
  --signaling-port "$SIGNALING_PORT" \
  --max-clients-per-call "$MAX_CLIENTS_PER_CALL"

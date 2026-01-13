#!/bin/bash
set -e
cd "$(dirname "$0")/.."

# Load configuration
source configs/frontend.env

echo "Starting Signal Calling Frontend..."
echo "Server Port: $SERVER_PORT"
echo "Connected to Backend at: $CALLING_SERVER_URL"
echo "Connected to DB at: $STORAGE_ENDPOINT"

./bin/calling_frontend \
  --server-ip "$SERVER_IP" \
  --server-port "$SERVER_PORT" \
  --calling-server-url "$CALLING_SERVER_URL" \
  --storage-table "$STORAGE_TABLE" \
  --storage-region "$STORAGE_REGION" \
  --storage-endpoint "$STORAGE_ENDPOINT" \
  --authentication-key "$AUTHENTICATION_KEY" \
  --zkparams "$ZKPARAMS" \
  --region "$REGION" \
  --version "$VERSION" \
  --regional-url-template "$REGIONAL_URL_TEMPLATE" \
  --max-clients-per-call "$MAX_CLIENTS_PER_CALL" \
  --cleanup-interval-ms "$CLEANUP_INTERVAL_MS"

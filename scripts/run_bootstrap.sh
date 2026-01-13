#!/bin/bash
set -e
cd "$(dirname "$0")/.."

# Load configuration
source configs/bootstrap.env

echo "Starting Bootstrap with DynamoDB at $DYNAMODB_ENDPOINT..."

echo "Checking if DynamoDB is reachable..."
curl -s $DYNAMODB_ENDPOINT > /dev/null || { echo "Error: Cannot reach DynamoDB at $DYNAMODB_ENDPOINT"; exit 1; }

# Run the bootstrap binary
./target/debug/bootstrap

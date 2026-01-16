#!/bin/bash
set -e

HOST="$1"
REMOTE_DIR="~/obsidian-memory"

if [ -z "$HOST" ]; then
  echo "Usage: ./deploy.sh <host>"
  exit 1
fi

echo "Deploying to $HOST..."
scp docker/docker-compose.yml docker/Caddyfile "$HOST:$REMOTE_DIR/"
ssh "$HOST" "cd $REMOTE_DIR && docker compose pull && docker compose up -d"
echo "Done."

#!/bin/bash
# shardchain.sh — bootstrap a new Babble shard directory.
#
# Uses BABBLE_DIR env var when set (local / --no-docker mode).
# Falls back to /root/.babble which is the default Docker container path.
#
# For peersMode 3 (non-head nodes fetching from a remote server), the
# ENTITY_API_URL env var overrides the default production endpoint so that
# local triple-node setups can point at localhost instead of the external
# api.decillionai.com server.

BABBLE_HOME="${BABBLE_DIR:-/root/.babble}"
ENTITY_API_URL="${ENTITY_API_URL:-http://api.decillionai.com}"

storageRoot=${1:-"/home/keyhan/data"}
workchainId=${2:-"main"}
shardchainId=${3:-"shard-main"}
peersMode=${4:-"3"}

dest="${storageRoot}/chains/${workchainId}/${shardchainId}"

# Create new key-pair and place it in new conf directory
mkdir -p "$dest"

# clone crypto keys
cp "$BABBLE_HOME/key.pub"  "$dest/key.pub"  2>/dev/null || true
cp "$BABBLE_HOME/priv_key" "$dest/priv_key" 2>/dev/null || true

if [ "$peersMode" = "1" ]; then
    cp "$BABBLE_HOME/peers.genesis.json" "$dest/peers.genesis.json" 2>/dev/null || true
elif [ "$peersMode" = "2" ]; then
    cp "$BABBLE_HOME/peers.genesis.json" "$dest/peers.genesis.json" 2>/dev/null || true
    cp "$BABBLE_HOME/peers.genesis.json" "$dest/peers.json"         2>/dev/null || true
else
    # get genesis.peers.json from the entity API
    echo "Fetching peers.genesis.json"
    curl --trace dump \
        -H "Work-Chain-Id: ${workchainId}" \
        -H "Shard-Chain-Id: ${shardchainId}" \
        -s "${ENTITY_API_URL}:8079/genesispeers" > "$dest/peers.genesis.json"
    # get up-to-date peers.json
    echo "Fetching peers.json"
    curl --trace dump \
        -H "Work-Chain-Id: ${workchainId}" \
        -H "Shard-Chain-Id: ${shardchainId}" \
        -s "${ENTITY_API_URL}:8079/peers" > "$dest/peers.json"
fi

#!/bin/bash

# Load testing with ffplay to simulate real HLS video players
# This actually plays the stream, creating realistic load

# Configuration
BASE_URL="http://localhost"
STREAM_KEY="andeby-kommunestyresalen-talerstolen"
PLAYLIST_URL="${BASE_URL}/live/${STREAM_KEY}/index.m3u8"
CONCURRENT_PLAYERS=100
PLAYER_DURATION=20000000  # seconds each player runs

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

echo -e "${GREEN}FFplay HLS Load Test${NC}"
echo "======================================"
echo "URL: $PLAYLIST_URL"
echo "Concurrent players: $CONCURRENT_PLAYERS"
echo "Player duration: $PLAYER_DURATION seconds"
echo "======================================"
echo ""

# Array to store PIDs
declare -a PIDS

# Function to run a single ffplay instance
run_ffplay_client() {
    local client_id=$1
    local url="${PLAYLIST_URL}"

    echo -e "${YELLOW}[Player $client_id]${NC} Starting with session $session_id"

    # Run ffplay in background with:
    # -nodisp: no video window (headless)
    # -loglevel warning: reduce output noise
    # -t: duration to play
    # -autoexit: exit when done
    ffplay \
        -nodisp \
        -loglevel warning \
        -t $PLAYER_DURATION \
        -autoexit \
        -i "$url" \
        2>&1 | while read line; do
            if [[ ! -z "$line" ]]; then
                echo -e "${BLUE}[Player $client_id]${NC} $line"
            fi
        done &

    local pid=$!
    PIDS+=($pid)
    echo -e "${GREEN}[Player $client_id]${NC} Started with PID $pid"
}

# Function to monitor system resources
monitor_resources() {
    echo -e "${YELLOW}Monitoring system resources...${NC}"

    while true; do
        # Count connections to port 8080
        if command -v ss &> /dev/null; then
            CONNECTIONS=$(ss -tan 2>/dev/null | grep -E ':8080|:80' | wc -l)
            echo -e "${BLUE}[Monitor]${NC} Active HTTP connections: $CONNECTIONS"
        fi

        # Check server status
        if STATUS=$(curl -s "${BASE_URL}/status" 2>/dev/null); then
            ACTIVE_STREAMS=$(echo "$STATUS" | grep -o '"active_streams":\[[^]]*\]' | grep -o '{' | wc -l)
            REQUESTS=$(echo "$STATUS" | grep -o '"requests":[0-9]*' | grep -o '[0-9]*')
            echo -e "${BLUE}[Monitor]${NC} Active streams: $ACTIVE_STREAMS, Total requests: $REQUESTS"
        fi

        sleep 5

        # Check if any ffplay processes are still running
        local running=0
        for pid in "${PIDS[@]}"; do
            if kill -0 $pid 2>/dev/null; then
                running=$((running + 1))
            fi
        done

        if [ $running -eq 0 ]; then
            echo -e "${YELLOW}[Monitor]${NC} All players have finished"
            break
        else
            echo -e "${BLUE}[Monitor]${NC} Players still running: $running"
        fi
    done
}

# Check for ffplay
if ! command -v ffplay &> /dev/null; then
    echo -e "${RED}ffplay not found. Please install ffmpeg package.${NC}"
    exit 1
fi

# Get initial server status
echo "Initial server status:"
curl -s "${BASE_URL}/health" | head -c 200
echo ""
echo ""

START_TIME=$(date +%s)

# Start resource monitoring in background
monitor_resources &
MONITOR_PID=$!

# Start all ffplay instances
echo -e "${GREEN}Starting $CONCURRENT_PLAYERS concurrent players...${NC}"
for i in $(seq 1 $CONCURRENT_PLAYERS); do
    run_ffplay_client $i
    # Small delay between starts to avoid thundering herd
    sleep 0.2
done

echo ""
echo -e "${GREEN}All players started. Waiting for completion...${NC}"
echo ""

# Wait for all ffplay processes
for pid in "${PIDS[@]}"; do
    wait $pid 2>/dev/null
done

# Stop monitoring
kill $MONITOR_PID 2>/dev/null

END_TIME=$(date +%s)
TOTAL_TIME=$((END_TIME - START_TIME))

echo ""
echo "======================================"
echo -e "${GREEN}Load test completed in $TOTAL_TIME seconds${NC}"
echo "======================================"

# Final server status
echo ""
echo "Final server status:"
HEALTH=$(curl -s "${BASE_URL}/health")
echo "$HEALTH" | head -c 300

echo ""
echo ""
echo "Server statistics:"
STATUS=$(curl -s "${BASE_URL}/status")
echo "Total requests: $(echo "$STATUS" | grep -o '"requests":[0-9]*' | grep -o '[0-9]*')"
echo "Started streams: $(echo "$STATUS" | grep -o '"started":[0-9]*' | grep -o '[0-9]*')"
echo "Stopped streams: $(echo "$STATUS" | grep -o '"stopped":[0-9]*' | grep -o '[0-9]*')"
echo "Failed streams: $(echo "$STATUS" | grep -o '"failed":[0-9]*' | grep -o '[0-9]*')"

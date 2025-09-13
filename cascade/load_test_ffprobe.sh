#!/bin/bash

# Load testing with ffprobe to simulate real HLS clients
# This actually validates the HLS stream and segments

# Configuration
BASE_URL="http://localhost"
STREAM_KEY="andeby-kommunestyresalen-talerstolen"
PLAYLIST_URL="${BASE_URL}/live/${STREAM_KEY}/index.m3u8"
CONCURRENT_CLIENTS=10
DURATION_SECONDS=20

# Colors
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo -e "${GREEN}FFprobe HLS Load Test${NC}"
echo "======================================"
echo "URL: $PLAYLIST_URL"
echo "Concurrent clients: $CONCURRENT_CLIENTS"
echo "Duration: $DURATION_SECONDS seconds"
echo "======================================"
echo ""

# Function for each ffprobe client
run_ffprobe_client() {
    local client_id=$1
    local session_id=$(head -c 16 /dev/urandom | xxd -p)
    local url="${PLAYLIST_URL}?hls_ctx=${session_id}"

    echo -e "${YELLOW}[Client $client_id]${NC} Starting ffprobe with session $session_id"

    # Run ffprobe with timeout
    timeout $DURATION_SECONDS ffprobe \
        -v warning \
        -print_format json \
        -show_format \
        -show_streams \
        -analyzeduration 5000000 \
        -probesize 5000000 \
        "$url" 2>&1 | while read line; do
            echo -e "${GREEN}[Client $client_id]${NC} $line"
        done

    local exit_code=$?
    if [ $exit_code -eq 124 ]; then
        echo -e "${GREEN}[Client $client_id]${NC} Completed (timeout as expected)"
    elif [ $exit_code -eq 0 ]; then
        echo -e "${GREEN}[Client $client_id]${NC} Stream analysis successful"
    else
        echo -e "${RED}[Client $client_id]${NC} Failed with exit code $exit_code"
    fi
}

# Simple concurrent load test using curl
run_curl_bombardment() {
    echo -e "${YELLOW}Starting rapid curl requests...${NC}"

    for i in $(seq 1 $CONCURRENT_CLIENTS); do
        (
            local count=0
            local session_id=$(head -c 16 /dev/urandom | xxd -p)
            local url="${PLAYLIST_URL}?hls_ctx=${session_id}"

            while [ $count -lt 10 ]; do
                # Fetch playlist
                curl -s -o /dev/null -w "[Curl $i] Playlist: %{http_code} in %{time_total}s\n" "$url"

                # Fetch some segments
                for segment in $(curl -s "$url" | grep "\.ts$" | head -2); do
                    segment_url="${BASE_URL}${segment}"
                    curl -s -o /dev/null -w "[Curl $i] Segment: %{http_code} Size: %{size_download} Time: %{time_total}s\n" "$segment_url"
                done

                count=$((count + 1))
                sleep 0.5
            done
        ) &
    done

    wait
    echo -e "${GREEN}Curl bombardment complete${NC}"
}

# Check for required tools
if ! command -v ffprobe &> /dev/null; then
    echo -e "${RED}ffprobe not found. Please install ffmpeg package.${NC}"
    exit 1
fi

# Start the test
START_TIME=$(date +%s)

echo -e "${GREEN}Phase 1: Concurrent curl requests${NC}"
echo "======================================"
run_curl_bombardment

echo ""
echo -e "${GREEN}Phase 2: FFprobe stream analysis${NC}"
echo "======================================"

# Run ffprobe clients in background
for i in $(seq 1 $((CONCURRENT_CLIENTS / 2))); do
    run_ffprobe_client $i &
done

# Wait for all background jobs
wait

END_TIME=$(date +%s)
TOTAL_TIME=$((END_TIME - START_TIME))

echo ""
echo "======================================"
echo -e "${GREEN}Load test completed in $TOTAL_TIME seconds${NC}"

# Show connection stats
if command -v ss &> /dev/null; then
    CONNECTIONS=$(ss -tan | grep :8080 | wc -l)
    echo "Active connections on port 8080: $CONNECTIONS"
fi

# Check server health
echo ""
echo "Checking server health..."
health_response=$(curl -s "${BASE_URL}/health")
echo "Health: $health_response"

status_response=$(curl -s "${BASE_URL}/status" | head -c 200)
echo "Status: $status_response..."
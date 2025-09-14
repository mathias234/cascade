#!/bin/bash

# Load testing script for HLS streaming server
# Uses curl and parallel processing to simulate multiple concurrent clients

# Configuration
BASE_URL="http://localhost"
STREAM_KEY="andeby-kommunestyresalen-talerstolen"
PLAYLIST_URL="${BASE_URL}/live/${STREAM_KEY}/index.m3u8"
CONCURRENT_CLIENTS=50
DURATION_SECONDS=30
FETCH_INTERVAL=2  # How often each client fetches playlist (seconds)

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo -e "${GREEN}HLS Load Test Script${NC}"
echo "======================================"
echo "URL: $PLAYLIST_URL"
echo "Concurrent clients: $CONCURRENT_CLIENTS"
echo "Duration: $DURATION_SECONDS seconds"
echo "Fetch interval: $FETCH_INTERVAL seconds"
echo "======================================"
echo ""

# Function to simulate a single HLS client
simulate_client() {
    local client_id=$1
    local end_time=$(($(date +%s) + DURATION_SECONDS))
    local session_id=$(uuidgen | tr -d '-' | head -c 16)
    local playlist_url="${PLAYLIST_URL}?hls_ctx=${session_id}"

    echo -e "${YELLOW}[Client $client_id]${NC} Starting with session $session_id"

    while [ $(date +%s) -lt $end_time ]; do
        # Fetch the playlist
        playlist_response=$(curl -s -w "\n%{http_code}" "$playlist_url" 2>/dev/null)
        http_code=$(echo "$playlist_response" | tail -n1)
        playlist_content=$(echo "$playlist_response" | head -n -1)

        if [ "$http_code" == "200" ]; then
            echo -e "${GREEN}[Client $client_id]${NC} Playlist fetch OK ($(echo "$playlist_content" | wc -c) bytes)"

            # Parse and fetch segments
            segments=$(echo "$playlist_content" | grep "\.ts$" | head -3)  # Get first 3 segments

            for segment in $segments; do
                # Remove leading /live/ if present
                segment_path=$(echo "$segment" | sed 's|^/live/||')
                segment_url="${BASE_URL}/live/${segment_path}"

                # Fetch segment
                segment_start=$(date +%s%N)
                segment_response=$(curl -s -o /dev/null -w "%{http_code} %{size_download}" "$segment_url" 2>/dev/null)
                segment_end=$(date +%s%N)
                segment_time=$(( (segment_end - segment_start) / 1000000 ))  # Convert to ms

                segment_code=$(echo "$segment_response" | awk '{print $1}')
                segment_size=$(echo "$segment_response" | awk '{print $2}')

                if [ "$segment_code" == "200" ]; then
                    echo -e "${GREEN}[Client $client_id]${NC} Segment: $(basename "$segment") - ${segment_size} bytes in ${segment_time}ms"
                else
                    echo -e "${RED}[Client $client_id]${NC} Segment failed: $(basename "$segment") - HTTP $segment_code"
                fi
            done
        else
            echo -e "${RED}[Client $client_id]${NC} Playlist fetch failed: HTTP $http_code"
        fi

        sleep $FETCH_INTERVAL
    done

    echo -e "${YELLOW}[Client $client_id]${NC} Finished"
}

# Export the function so parallel can use it
export -f simulate_client
export BASE_URL STREAM_KEY PLAYLIST_URL DURATION_SECONDS FETCH_INTERVAL
export RED GREEN YELLOW NC

# Check if GNU parallel is installed
if ! command -v parallel &> /dev/null; then
    echo -e "${RED}GNU parallel not found. Installing...${NC}"
    if command -v apt-get &> /dev/null; then
        sudo apt-get install -y parallel
    elif command -v brew &> /dev/null; then
        brew install parallel
    else
        echo -e "${RED}Please install GNU parallel manually${NC}"
        exit 1
    fi
fi

# Start time tracking
START_TIME=$(date +%s)

echo -e "${GREEN}Starting $CONCURRENT_CLIENTS concurrent clients...${NC}"
echo ""

# Run clients in parallel
seq 1 $CONCURRENT_CLIENTS | parallel -j $CONCURRENT_CLIENTS simulate_client {}

# Calculate total time
END_TIME=$(date +%s)
TOTAL_TIME=$((END_TIME - START_TIME))

echo ""
echo "======================================"
echo -e "${GREEN}Load test completed in $TOTAL_TIME seconds${NC}"
echo "======================================"

# Optional: Show system stats if available
if command -v ss &> /dev/null; then
    CONNECTIONS=$(ss -tan | grep :8080 | wc -l)
    echo "Active connections to port 8080: $CONNECTIONS"
fi
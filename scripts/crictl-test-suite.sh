#!/bin/bash
# Comprehensive crictl test suite for a3s-box CRI implementation

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Test counters
TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

# CRI socket path
CRI_SOCKET="/var/run/a3s-box/a3s-box.sock"

# Test result tracking
log_test() {
    echo -e "${YELLOW}[TEST]${NC} $1"
    TESTS_RUN=$((TESTS_RUN + 1))
}

log_pass() {
    echo -e "${GREEN}[PASS]${NC} $1"
    TESTS_PASSED=$((TESTS_PASSED + 1))
}

log_fail() {
    echo -e "${RED}[FAIL]${NC} $1"
    TESTS_FAILED=$((TESTS_FAILED + 1))
}

# Check prerequisites
check_prerequisites() {
    log_test "Checking prerequisites"

    if ! command -v crictl &> /dev/null; then
        log_fail "crictl not found. Please install crictl first."
        exit 1
    fi

    if [ ! -S "$CRI_SOCKET" ]; then
        log_fail "CRI socket not found at $CRI_SOCKET"
        echo "Please start a3s-box-cri first:"
        echo "  sudo ./target/release/a3s-box-cri --socket $CRI_SOCKET"
        exit 1
    fi

    log_pass "Prerequisites check"
}

# Test 1: Version
test_version() {
    log_test "Testing CRI version"

    if crictl --runtime-endpoint "unix://$CRI_SOCKET" version &> /dev/null; then
        log_pass "CRI version"
    else
        log_fail "CRI version"
    fi
}

# Test 2: Info
test_info() {
    log_test "Testing CRI info"

    if crictl --runtime-endpoint "unix://$CRI_SOCKET" info &> /dev/null; then
        log_pass "CRI info"
    else
        log_fail "CRI info"
    fi
}

# Test 3: Pull image
test_pull_image() {
    log_test "Testing image pull"

    if crictl --runtime-endpoint "unix://$CRI_SOCKET" pull docker.io/library/alpine:latest &> /dev/null; then
        log_pass "Image pull"
    else
        log_fail "Image pull"
    fi
}

# Test 4: List images
test_list_images() {
    log_test "Testing image list"

    if crictl --runtime-endpoint "unix://$CRI_SOCKET" images | grep -q alpine; then
        log_pass "Image list"
    else
        log_fail "Image list"
    fi
}

# Test 5: Create pod sandbox
test_create_pod() {
    log_test "Testing pod sandbox creation"

    cat > /tmp/pod-config.json <<EOF
{
    "metadata": {
        "name": "test-pod",
        "namespace": "default",
        "uid": "test-pod-uid"
    },
    "log_directory": "/tmp",
    "linux": {}
}
EOF

    POD_ID=$(crictl --runtime-endpoint "unix://$CRI_SOCKET" runp /tmp/pod-config.json 2>&1)

    if [ $? -eq 0 ]; then
        log_pass "Pod sandbox creation"
        echo "$POD_ID" > /tmp/test-pod-id
    else
        log_fail "Pod sandbox creation"
    fi
}

# Test 6: List pods
test_list_pods() {
    log_test "Testing pod list"

    if crictl --runtime-endpoint "unix://$CRI_SOCKET" pods | grep -q test-pod; then
        log_pass "Pod list"
    else
        log_fail "Pod list"
    fi
}

# Test 7: Create container
test_create_container() {
    log_test "Testing container creation"

    if [ ! -f /tmp/test-pod-id ]; then
        log_fail "Container creation (no pod)"
        return
    fi

    POD_ID=$(cat /tmp/test-pod-id)

    cat > /tmp/container-config.json <<EOF
{
    "metadata": {
        "name": "test-container"
    },
    "image": {
        "image": "docker.io/library/alpine:latest"
    },
    "command": ["/bin/sh", "-c", "sleep 3600"],
    "linux": {}
}
EOF

    CONTAINER_ID=$(crictl --runtime-endpoint "unix://$CRI_SOCKET" create "$POD_ID" /tmp/container-config.json /tmp/pod-config.json 2>&1)

    if [ $? -eq 0 ]; then
        log_pass "Container creation"
        echo "$CONTAINER_ID" > /tmp/test-container-id
    else
        log_fail "Container creation"
    fi
}

# Test 8: Start container
test_start_container() {
    log_test "Testing container start"

    if [ ! -f /tmp/test-container-id ]; then
        log_fail "Container start (no container)"
        return
    fi

    CONTAINER_ID=$(cat /tmp/test-container-id)

    if crictl --runtime-endpoint "unix://$CRI_SOCKET" start "$CONTAINER_ID" &> /dev/null; then
        log_pass "Container start"
    else
        log_fail "Container start"
    fi
}

# Test 9: List containers
test_list_containers() {
    log_test "Testing container list"

    if crictl --runtime-endpoint "unix://$CRI_SOCKET" ps | grep -q test-container; then
        log_pass "Container list"
    else
        log_fail "Container list"
    fi
}

# Test 10: Container stats
test_container_stats() {
    log_test "Testing container stats"

    if [ ! -f /tmp/test-container-id ]; then
        log_fail "Container stats (no container)"
        return
    fi

    CONTAINER_ID=$(cat /tmp/test-container-id)

    if crictl --runtime-endpoint "unix://$CRI_SOCKET" stats "$CONTAINER_ID" &> /dev/null; then
        log_pass "Container stats"
    else
        log_fail "Container stats"
    fi
}

# Test 11: Exec in container
test_exec() {
    log_test "Testing container exec"

    if [ ! -f /tmp/test-container-id ]; then
        log_fail "Container exec (no container)"
        return
    fi

    CONTAINER_ID=$(cat /tmp/test-container-id)

    if crictl --runtime-endpoint "unix://$CRI_SOCKET" exec "$CONTAINER_ID" echo "test" | grep -q "test"; then
        log_pass "Container exec"
    else
        log_fail "Container exec"
    fi
}

# Cleanup
cleanup() {
    log_test "Cleaning up test resources"

    if [ -f /tmp/test-container-id ]; then
        CONTAINER_ID=$(cat /tmp/test-container-id)
        crictl --runtime-endpoint "unix://$CRI_SOCKET" stop "$CONTAINER_ID" &> /dev/null || true
        crictl --runtime-endpoint "unix://$CRI_SOCKET" rm "$CONTAINER_ID" &> /dev/null || true
        rm /tmp/test-container-id
    fi

    if [ -f /tmp/test-pod-id ]; then
        POD_ID=$(cat /tmp/test-pod-id)
        crictl --runtime-endpoint "unix://$CRI_SOCKET" stopp "$POD_ID" &> /dev/null || true
        crictl --runtime-endpoint "unix://$CRI_SOCKET" rmp "$POD_ID" &> /dev/null || true
        rm /tmp/test-pod-id
    fi

    rm -f /tmp/pod-config.json /tmp/container-config.json

    log_pass "Cleanup"
}

# Main test execution
main() {
    echo "========================================="
    echo "  a3s-box CRI Test Suite"
    echo "========================================="
    echo ""

    check_prerequisites

    echo ""
    echo "Running tests..."
    echo ""

    test_version
    test_info
    test_pull_image
    test_list_images
    test_create_pod
    test_list_pods
    test_create_container
    test_start_container
    test_list_containers
    test_container_stats
    test_exec

    echo ""
    echo "Cleaning up..."
    cleanup

    echo ""
    echo "========================================="
    echo "  Test Results"
    echo "========================================="
    echo "Total tests: $TESTS_RUN"
    echo -e "${GREEN}Passed: $TESTS_PASSED${NC}"
    echo -e "${RED}Failed: $TESTS_FAILED${NC}"
    echo ""

    if [ $TESTS_FAILED -eq 0 ]; then
        echo -e "${GREEN}All tests passed!${NC}"
        exit 0
    else
        echo -e "${RED}Some tests failed.${NC}"
        exit 1
    fi
}

# Run main
main

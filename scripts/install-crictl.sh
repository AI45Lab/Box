#!/bin/bash
# Install crictl for testing CRI implementation

set -e

VERSION="v1.30.0"
ARCH=$(uname -m)
OS=$(uname -s | tr '[:upper:]' '[:lower:]')

if [ "$ARCH" = "x86_64" ]; then
    ARCH="amd64"
elif [ "$ARCH" = "aarch64" ] || [ "$ARCH" = "arm64" ]; then
    ARCH="arm64"
fi

echo "Installing crictl ${VERSION} for ${OS}-${ARCH}..."

# Download crictl
DOWNLOAD_URL="https://github.com/kubernetes-sigs/cri-tools/releases/download/${VERSION}/crictl-${VERSION}-${OS}-${ARCH}.tar.gz"
TMP_DIR=$(mktemp -d)
cd "$TMP_DIR"

echo "Downloading from ${DOWNLOAD_URL}..."
curl -L "$DOWNLOAD_URL" -o crictl.tar.gz

# Extract and install
tar xzf crictl.tar.gz
sudo mv crictl /usr/local/bin/
sudo chmod +x /usr/local/bin/crictl

# Cleanup
cd -
rm -rf "$TMP_DIR"

# Verify installation
crictl --version

echo "crictl installed successfully!"

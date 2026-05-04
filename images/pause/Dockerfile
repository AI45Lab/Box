# A3S Box Pause Container
# Minimal sandbox/pause container for Kubernetes CRI
#
# This container serves as the infrastructure container for Kubernetes pods.
# It holds the network namespace and keeps the pod alive.

FROM alpine:3.19

# Install minimal dependencies
RUN apk add --no-cache tini

# Create a simple pause binary that just sleeps forever
RUN echo '#!/bin/sh' > /pause && \
    echo 'exec tini -s -- sleep infinity' >> /pause && \
    chmod +x /pause

# Metadata
LABEL maintainer="A3S Box Team"
LABEL description="Pause container for A3S Box CRI"
LABEL version="1.0.0"

# Run as non-root user for security
USER 65534:65534

# The pause command
ENTRYPOINT ["/pause"]

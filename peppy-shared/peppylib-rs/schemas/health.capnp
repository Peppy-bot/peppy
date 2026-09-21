@0xa8f5c2b9e4d3f1a8;

# Health message structures for core-node services

struct NodeHealthRequest {
    # Empty for now - could add specific health check parameters later
}

struct NodeHealthResponse {
    # Empty for now - presence of response indicates healthy
}

struct NodeReadyRequest {
    # Empty for now - core node polls this to check if node's runner::run() has started
}

struct NodeReadyResponse {
    # Empty for now - presence of response indicates node is ready
}

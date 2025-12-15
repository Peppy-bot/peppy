@0xa8f5c2b9e4d3f1aa;

# Node message structures for master-node services

enum NodeCmd {
    add @0;
    list @1;
    synchronize @2;
}

struct NodeRequest {
    cmd @0 :NodeCmd;
}

struct NodeResponse {
    cmd @0 :NodeCmd;
    value @1 :Text;
}

struct AddNodeRequest {
    # Name of the node to add
    nodeName @0 :Text;
    # Node type/role
    nodeType @1 :Text;
    # Node address
    address @2 :Text;
}

struct AddNodeResponse {
    # Whether the node was added successfully
    success @0 :Bool;
    # Assigned node ID
    nodeId @1 :Text;
    # Error message if failed
    errorMessage @2 :Text;
}

@0xd3d201f00b06c9a6;

# Datastore service messages. Keys are arbitrary strings carried in the
# payload (not a Zenoh keyexpr), so any character is allowed. Values are
# arbitrary bytes plus a Zenoh-style encoding tag (e.g. "text/plain",
# "application/json", "application/octet-stream").

struct DatastoreStoreRequest {
    key      @0 :Text;
    value    @1 :Data;
    encoding @2 :Text;
}

struct DatastoreStoreResponse {
}

struct DatastoreGetRequest {
    key @0 :Text;
}

struct DatastoreGetResponse {
    # Whether a value was found for the requested key. When false, `value`
    # and `encoding` are empty.
    found    @0 :Bool;
    value    @1 :Data;
    encoding @2 :Text;
}

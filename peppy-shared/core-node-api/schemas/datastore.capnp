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
    # Whether a value was found for the requested key. When false, `value`,
    # `encoding` and `lastModifiedBy` are empty.
    found          @0 :Bool;
    value          @1 :Data;
    encoding       @2 :Text;
    # instance_id of the node that last wrote this key (empty when not found).
    lastModifiedBy @3 :Text;
}

# Lists every key currently in the store. Takes no arguments: the whole
# keyspace is returned.
struct DatastoreListRequest {
}

# A single key's metadata in a list response. The value bytes are intentionally
# omitted (fetch them with DatastoreGetRequest); a list stays cheap regardless
# of how large the stored values are.
struct DatastoreListEntry {
    key            @0 :Text;
    encoding       @1 :Text;
    lastModifiedBy @2 :Text;
}

struct DatastoreListResponse {
    entries @0 :List(DatastoreListEntry);
}

# Removes (unsets) a single key.
struct DatastoreRemoveRequest {
    key @0 :Text;
}

struct DatastoreRemoveResponse {
    # True if the key existed and was removed, false if it was already absent.
    removed @0 :Bool;
}

# The answer to `datastore_store_exclusive`, whose request is a
# DatastoreStoreRequest: the service stores a key that is absent or the
# caller's own and makes the calling instance its owner, unless another
# instance owns it; a key that exists with no owner is an error. The core node
# then refuses every other instance's store and remove of the key, and removes
# the key when the owner's process is gone.
struct DatastoreStoreExclusiveResponse {
    union {
        # Discriminant zero: a message that never set an outcome decodes as
        # `unset`, which the decoder refuses.
        unset  @0 :Void;
        stored @1 :Void;
        # Another instance owns the key; the key as its owner wrote it.
        refused :group {
            value          @2 :Data;
            encoding       @3 :Text;
            lastModifiedBy @4 :Text;
        }
    }
}

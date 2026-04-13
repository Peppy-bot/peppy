@0xab294fbc5fd0a02f;

# Repo message structures for core-node services

struct RepoAddGitSource {
    # URL of the git repository
    repoUrl @0 :Text;
    # Optional git ref (tag/branch/commit) to checkout
    repoRef @1 :Text;
}

struct RepoAddRequest {
    # Source of the repository to add
    source :union {
        # Git repository source
        git @0 :RepoAddGitSource;
        # Plain URL source
        url @1 :Text;
        # Local filesystem path
        fs @2 :Text;
    }
}

struct RepoAddResponse {
    # Whether the add was successful
    success @0 :Bool;
    # Error message if failed (optional)
    errorMessage @1 :Text;
}

# ── Repo Refresh (action with feedback) ──────────────────────────

struct RepoRefreshGoal {
    # Empty for now — refresh all repos.
}

struct RepoRefreshGoalResponse {
    accepted @0 :Bool;
    rejectionReason @1 :Text;
}

struct RepoRefreshFeedback {
    nodeName @0 :Text;
    nodeTag @1 :Text;
    # "fs", "git", or "url"
    sourceType @2 :Text;
    # Absolute path (fs) or relative path within repo (git)
    path @3 :Text;
    # Variant names declared by this node (empty if none)
    variants @4 :List(Text);
}

struct RepoRefreshResult {
    success @0 :Bool;
    errorMessage @1 :Text;
    totalNodesFound @2 :UInt32;
}

# ── Repo List (request-response) ────────────────────────────────

struct RepoListRequest {
    # Empty — list all repositories and their nodes.
}

struct RepoListNodeEntry {
    nodeName @0 :Text;
    nodeTag @1 :Text;
    # "fs", "git", or "url"
    sourceType @2 :Text;
    # Absolute path (fs) or relative path within repo (git)
    path @3 :Text;
    # Variant names declared by this node (empty if none)
    variants @4 :List(Text);
}

struct RepoListResponse {
    success @0 :Bool;
    errorMessage @1 :Text;
    nodes @2 :List(RepoListNodeEntry);
}

# ── Repo Remove (request-response) ─────────────────────────────

struct RepoRemoveRequest {
    # ID of the repository to remove
    id @0 :UInt32;
}

struct RepoRemoveResponse {
    # Whether the removal was successful
    success @0 :Bool;
    # Error message if failed (optional)
    errorMessage @1 :Text;
}

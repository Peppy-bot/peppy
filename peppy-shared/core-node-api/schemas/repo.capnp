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
        # Local filesystem path
        fs @1 :Text;
    }
    # When true, assign the new repo an id below the current minimum so it
    # takes top priority. Defaults to false (append with max+1).
    top @2 :Bool;
    # How to assign the new repository's id. `auto` (the default, and what
    # every pre-id message on the wire decodes as) keeps the historical
    # behavior: max+1, or min-1 when top=true. `explicit` pins the caller's
    # chosen id, so organization setups can register repositories from a
    # reserved band (>= 2000) whose ids can never collide with a default a
    # future peppy release ships.
    id :union {
        auto @3 :Void;
        explicit @4 :UInt64;
    }
}

# The response of a repository add that succeeded. A failed add is an
# error reply, with a failure kind and a message.
struct RepoAddResponse {
}

# ── Repo Refresh (action with feedback) ──────────────────────────

struct RepoRefreshGoal {
    # Empty: refresh all repos.
}

# The goal response of an accepted refresh. The daemon refuses a goal in
# the goal envelope, with a failure kind and a message.
struct RepoRefreshGoalResponse {
}

struct RepoRefreshFeedback {
    payload :union {
        # A discovered node, launcher, contract, pairing, or MCP exposure manifest.
        discovered :group {
            # Kind of item being reported: "node", "launcher", "contract",
            # "pairing", or "mcp_exposure".
            kind @0 :Text;
            # Name of the discovered item.
            itemName @1 :Text;
            # Tag of the discovered item. Empty for launchers (which have no tag).
            itemTag @2 :Text;
            # "fs" or "git"
            sourceType @3 :Text;
            # Absolute path (fs) or relative path within repo (git). Points
            # at the manifest file itself.
            path @4 :Text;
            # SHA-256 of the manifest file bytes.
            sha256 @5 :Text;
        }
        # A repository that was skipped (e.g. listed in excluded_repositories.json5).
        excluded :group {
            # "fs" or "git"
            sourceType @6 :Text;
            # Repository identity (URL or fs path).
            identity @7 :Text;
        }
        # A free-form status update emitted during the scan (e.g. "Cloning <url>").
        progress @8 :Text;
    }
}

# The result of a completed refresh: it ran, it published the caches, and
# every repository that read cleanly is current. A failed refresh gives a
# failure kind and a message, and no result; a cancelled refresh publishes
# nothing and gives its reason, and no result.
struct RepoRefreshResult {
    totalNodesFound @0 :UInt32;
    totalLaunchersFound @1 :UInt32;
    totalContractsFound @2 :UInt32;
    totalPairingsFound @3 :UInt32;
    totalMcpExposuresFound @4 :UInt32;
    # The repositories that could not be read this run, each still serving
    # the entries it last published, and why. Empty when every configured
    # repository was read.
    readFailureReport @5 :Text;
}

# ── Repo List (request-response) ────────────────────────────────

struct RepoListRequest {
    # Empty: list all repositories and their nodes.
}

struct RepoListNodeEntry {
    nodeName @0 :Text;
    nodeTag @1 :Text;
    # "fs" or "git"
    sourceType @2 :Text;
    # Absolute path (fs) or relative path within repo (git)
    path @3 :Text;
    # True when another repo with higher priority already provides this node.
    # Cross-repository shadowing: a supported feature with a documented
    # order, where the lower-id repository deterministically wins.
    duplicate @4 :Bool;
    # Id of the owning repository (from repositories.json5)
    repoId @5 :UInt32;
    # Display label of the owning repository (path for fs, "url (ref: r)" for git)
    repoLabel @6 :Text;
}

# Per-repository read status, so a partial update is legible: which
# repositories are current, which are serving entries retained from an
# earlier read, and why.
struct RepoListRepoEntry {
    id @0 :UInt32;
    # Display label (path for fs, "url (ref: r)" for git)
    label @1 :Text;
    # "fs" or "git"
    sourceType @2 :Text;
    # Unix seconds of the last read that produced entries; 0 when this
    # repository has never been read successfully on this machine.
    lastReadUnixSecs @3 :UInt64;
    # True when the entries listed for this repository come from an
    # earlier read because its most recent one failed.
    retained @4 :Bool;
    # The read failure of the last read: "" when the last read succeeded,
    # otherwise "unreachable" or "conflict". An outage and a content bug
    # send the user to completely different places, so they are never
    # collapsed into one label.
    readFailure @5 :Text;
    readFailureDetail @6 :Text;
}

# The response of a listing that succeeded. A failed listing is an error
# reply, with a failure kind and a message.
struct RepoListResponse {
    nodes @0 :List(RepoListNodeEntry);
    repos @1 :List(RepoListRepoEntry);
}

# ── Repo Remove (request-response) ─────────────────────────────

struct RepoRemoveRequest {
    # ID of the repository to remove
    id @0 :UInt64;
}

# The response of a removal whose configuration edit landed. A failed
# removal is an error reply, with a failure kind and a message.
struct RepoRemoveResponse {
    # Problems from the re-index that follows the change: whether the
    # re-read that made it take effect worked. Empty when it did.
    refreshReport @0 :Text;
}

# ── Repo Exclude (request-response) ───────────────────────────

struct RepoExcludeRequest {
    # Source of the repository to exclude
    source :union {
        # Git repository source
        git @0 :RepoAddGitSource;
        # Local filesystem path
        fs @1 :Text;
    }
}

# The response of an exclusion whose configuration edit landed. A failed
# exclusion is an error reply, with a failure kind and a message.
struct RepoExcludeResponse {
    # Problems from the re-index that follows the change: whether the
    # re-read that made it take effect worked. Empty when it did. This
    # matters most on the recovery path, where the user excluded a
    # repository precisely to unblock themselves and needs to know whether
    # it worked.
    refreshReport @0 :Text;
}

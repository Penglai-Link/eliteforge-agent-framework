# storage-local

`eliteforge-agent-official-plugin-storage-local` is the Framework's local-first
storage implementation. It uses one SQLite database for execution checkpoints,
interactions, conversations, long-term memory, summaries, model catalog
snapshots, and artifact metadata. Artifact bytes live in a host-provided
filesystem directory.

```rust
use eliteforge_agent_official_plugin_storage_local::LocalStoragePlugin;

// The host must create this directory and protect it before installation.
let storage = LocalStoragePlugin::new("agent.db", "agent-artifacts");
```

On Unix, the artifact directory must be private to its owner (mode 0700).
On Windows, the host is responsible for equivalent ACL protection. The plugin
never creates that directory. Artifact IDs are identifiers, not access tokens;
do not share a store across tenants that must be isolated.

File payloads are fsynced and published atomically before the SQLite metadata
row is committed. A metadata failure rolls the file back. Deletes remove
metadata first, then unlink the file. An interrupted operation may leave a
file with no metadata; reopening the store sweeps unreferenced files older
than 24 hours, and `LocalArtifactStore::collect_orphans` can be called
explicitly. The legacy `agent_artifacts` SQLite BLOB table is not read or
migrated. Its former IDs return `ArtifactError::NotFound`.

Model catalog snapshots hold provider metadata only. Runtime adapters and
authorization data are reconstructed by the integration on restore; tokens,
passwords, and sessions do not belong in the snapshot.

This implementation targets a single machine with an embedded database and
filesystem. It does not provide distributed consistency or multi-node HA.

Session deletion is available through the installed `SessionStoreService` and
typed Agent operations:

```rust,ignore
use eliteforge_agent_official_plugin_storage_local::{DeleteSession, DeleteSessions};

let deleted: bool = agent.execute(DeleteSession("chat-1".into())).await?;
let report = agent
    .execute(DeleteSessions(vec!["chat-2".into(), "chat-3".into()]))
    .await?;
// report.deleted counts removed sessions.
// report.failures contains each failed session_id and its StateError.
```

Both `LocalStoragePlugin` and `SqliteExecutionPlugin` provide this service.
`SqliteStateStore` also implements the public `SessionStore` contract for direct
calls to `delete_session` / `delete_sessions`. Existing custom execution and
conversation stores do not need to implement this separate contract.

Deletion removes all checkpoints (including embedded transcripts and events),
the session head, pending and answered interactions, every conversation branch,
and summaries from every generation of that session. IDs match exactly; blank
IDs are rejected. Missing sessions return false, duplicate batch IDs count
once, and an empty batch returns an empty report. Each session uses its own
SQLite write transaction. Batch deletion continues after failures and returns
`SessionDeletionReport { deleted, failures }`; successes remain committed.
Each failure includes the exact session ID and a typed `StateError`. SQL errors
roll back only that session's cleanup. Live execution/conversation leases return
`StateError::Busy`; expired leases do not block cleanup. A missing ID is a
successful no-op and does not increase `deleted` or enter `failures`.

The host must authorize these administrative operations and stop runs and
background summary jobs before deletion. Deletion does not tombstone IDs:
later writers may intentionally recreate the session. Long-term memories,
model catalogs and artifact files/metadata are shared or independently keyed,
so they remain available through their own lifecycle APIs.

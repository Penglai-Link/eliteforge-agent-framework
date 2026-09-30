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

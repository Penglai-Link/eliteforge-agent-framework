# storage-local

`eliteforge-agent-official-plugin-storage-local` is the Framework's local-first
storage implementation. It uses one SQLite runtime for sessions, goals, runs,
events, execution checkpoints, interactions, conversations, long-term memory, summaries, model catalog
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
explicitly. The schema uses `ef_*` tables. Development databases must be
recreated; historical schemas and API compatibility are not supported.

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

Deletion removes the Session, its Goals and criteria, Runs, Events, context,
artifact references, every checkpoint scope, interaction, conversation branch,
and summary generation. Native child sessions owned by the root session belong
to the same deletion boundary. Exact IDs are used throughout. Blank IDs fail;
missing IDs return false. Live run, checkpoint and conversation leases return
`StateError::Busy`. Expired executions are marked `RECOVERY_REQUIRED` on startup;
the affected goal is paused and external side effects are never replayed automatically.

Each deletion and its database cleanup are atomic. Batch deletion continues
past individual failures. When called inside an outer transaction, all batch
successes join that transaction and a database error makes it rollback-only.
Deleted IDs retain only a global admission tombstone; late checkpoint,
conversation, summary and run writes cannot recreate them.

Owned artifacts lose metadata when their final session reference disappears.
File removal runs after the outermost commit and is best effort. A durable
cleanup queue retries failures on startup or subsequent deletion. Shared
long-term memories, catalog snapshots and independently owned artifacts retain
their own lifecycle. Hosts authorize deletion and stop their background work.

## Sharing the application's database

```rust,ignore
use eliteforge_agent_official_plugin_storage_local::{SqliteDatabase, LocalStoragePlugin};

let database = SqliteDatabase::open("application.db").await?;
let framework = Framework::builder()
    .plugin(LocalStoragePlugin::new("application.db", "artifacts")
        .with_database(database.clone()))
    .build().await?;
```

Inject the same `SqliteDatabase` handle into host repositories. Opening another
connection to the same file does not join the transaction. `Framework::transaction`
and `SqliteDatabase::transaction` use REQUIRED semantics: a nested call joins the
current Tokio task's transaction on the same connection, and only the outermost
call commits or rolls back. An inner error makes the transaction rollback-only,
even if the caller catches it. Cancellation rolls back before admitting unrelated
work. Newly spawned tasks do not inherit transaction context. Avoid waiting for
spawned database work while holding the transaction; run dependent writes in the
same task. Do not wrap an entire Agent execution or a remote call in a database
transaction.

The plugin closes a runtime it opens itself. With `with_database`, the application
owns runtime shutdown and can continue using its business tables after Framework
shutdown. Individual stores never close the injected connection.

See [Session and Goal API](../../../../docs/会话与目标存储.md) for lifecycle
and continuation examples.

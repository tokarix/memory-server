# Rust storage policy conversion

The [pinned manifest](manifests/rust-storage-2026-09-26.json) converts three
legacy general Rules into two profile-specific mandatory storage policies and
storage-neutral contextual Woodpecker guidance. It was prepared from the
deployed #80 HTTP GET representation on 2026-09-26. It includes the complete
persisted source content, summary, tags, UUID, and full RFC3339
updated_at token. The command never refreshes these preconditions itself.

| Source UUID | Conversion | Active result |
| --- | --- | --- |
| eaeb57ed-95b4-486f-89db-f3327db91468 | Adopt as host revision 1 | Superseded by host revision 2 |
| 809cbf86-cd77-41a4-af56-0fccbd8b18ba | Adopt as host revision 2 | Superseded by curated host revision 3 |
| 7404e1ce-1058-46b3-9c19-53235735e708 | Adopt as CI guidance revision 1 | Superseded by storage-neutral revision 2 |

The active host revision is
general/rust.build.storage.workstation, mandatory for
profile=workstation-host and language=rust. It requires disk-backed Cargo
artifacts and compiler temporary files, with explicit CARGO_TARGET_DIR and
TMPDIR. The active container revision is
general/rust.build.storage.woodpecker, mandatory for
profile=woodpecker-container and language=rust. It requires
CARGO_TARGET_DIR=/tmp/target inside that isolated container only. Both
declare rust.build.target_storage, with different exact values in disjoint
domains. The active general/rust.ci.woodpecker guidance retains pipeline
structure, lint, and verification instructions; its normative content has no
unconditional storage path. Historical source bodies remain exactly
retrievable.

## Rollout

1. Deploy the compatible memoryd, MCP shim, and hooks first. Supply trusted
   execution context at the client boundary. Confirm both profiles can call
   rules and bootstrap before conversion.
2. Use a database configuration that targets the intended memoryd database
   and has the scope migration installed. Back up that database using the
   normal operator procedure. Run the command without --apply:

   ~~~sh
   cargo run -p memoryd --bin convert_storage_policies -- \
     --config /path/to/config.toml \
     docs/manifests/rust-storage-2026-09-26.json
   ~~~

3. Review all six proposed operations and both complete canonical policy
   previews. The host preview must contain only the workstation storage
   mandate; the container preview must contain only the Woodpecker storage
   mandate. Both may contain the storage-neutral CI guidance and other
   existing applicable Rules. Any stale source or policy conflict is a hard
   failure. Re-read and review a changed source before making a new manifest;
   do not silently replace its pinned timestamp.
4. Explicitly apply the same file by adding --apply to the command. The
   command prepares all three embeddings before opening the transaction,
   locks every affected policy identity in fixed order, locks all source rows
   in UUID order, rechecks their exact preconditions, writes all adoption and
   successor revisions, resolves both profiles within that transaction, and
   commits. A failure rolls back every change. Readers see either the old or
   the complete new set.
5. Run the dry run again. It must report already_applied=true and the same
   active UUIDs. An --apply replay also verifies the exact committed result
   without writing new revisions. A partial or different result fails.

The command intentionally does not migrate schemas or mutate the live service
through MCP. It uses the same checked policy publication and adoption helpers
as the API. A dry run performs database reads only. The manifest is a
deployment artifact; implementation tests use disposable PostgreSQL fixtures
with the same manifest and never apply it to live memories.

## Recovery

Before commit, any stale token, unexpected identity/head, invalid selector,
database error, or policy conflict aborts the transaction. Inspect the error
and the source/target records before another attempt. After commit, these
classified revisions are immutable. Do not delete or edit the historical
records or use the schema down migration: it refuses to discard scoped data.
If policy wording needs correction, publish explicit successors under the
same keys. Restore a database backup only through the normal coordinated
recovery procedure.

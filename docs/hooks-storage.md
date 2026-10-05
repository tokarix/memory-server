# Filesystem-backed storage assessment

The Linux `memory_hooks::storage` library assesses explicit directory candidates
against the authoritative structured registry. The [managed Rust gate](hooks-rust-build.md)
resolves complete candidates, proves local execution, evaluates them after a
fresh mandatory pack comparison, persists audit v3 evidence and conditionally
retires denied or stale claims. Evaluation is point-in-time evidence; neutral
output continues to native client permissions.

## Calling contract

Supply a `ResolvedBinding`, the authoritative `GuardrailPack`, explicit typed
directory candidates, and `ExecutionDestination::SameNamespaceAndRoot` only
after establishing that the pending operation will run in this same mount
namespace and process root. A remote operation, future container entry, chroot,
setns, or unknown destination is Indeterminate. `storage::evaluate` returns a
bounded redacted report, including scope failures. Alternatively,
`EvaluationScope::observe` validates policy before opening and holding execution
descriptors; consume that scope immediately with `evaluate`.

Candidates retain OS-native bytes. An absolute path is interpreted in this
execution namespace; a relative path uses only the trusted binding's effective
cwd. There is no ambient cwd, HOME/TMPDIR expansion, shell or Cargo syntax.
`BuildOutput` means an explicit output **directory**. Individual output files
require a future typed extension. `SourceWorktree` must already exist.

The registry matches exact structured key/value pairs:

| Constraint | Roles | Required evidence |
| --- | --- | --- |
| `rust.build.target_storage=persistent-disk` | TargetDirectory, BuildOutput, SourceWorktree | Supported filesystem and all recognized nonvolatile device leaves |
| `rust.build.compiler_tmp_storage=persistent-disk` | CompilerTemporary | The same positive persistence evidence |
| `rust.build.target_storage=container-local-tmp` | TargetDirectory, BuildOutput | Trusted woodpecker-container scope, protected execution pins, exact local mount provenance, and `/tmp/target` containment |

Mapping persistent target storage to source worktrees is the explicit version-one
validator contract, not a deduction from policy prose. Container compiler/source
roles remain NotApplicable unless another applicable storage policy constrains
them. Every authoritative applicable rule is preserved; this library never
resolves overrides or chooses a winner. Conflicting target declarations,
unsupported storage keys/values in the `rust.build` storage namespace, and
missing/mismatched required selectors cannot Pass. Unknown values remain private.
Prose-only edits change the pack digest, but not the storage classification.

Pass needs positive evidence for every applicable assessment. Deny is a proven
violation, including volatile host backing or a forbidden container location.
Missing, inaccessible, unsupported, incoherent, excessive or changed evidence is
Indeterminate. Neither Deny nor Indeterminate satisfies a mandatory requirement.
Aggregation uses Deny > Indeterminate > Pass; all-NotApplicable stays
NotApplicable. An empty candidate set with storage requirements is Indeterminate.
Per-candidate roles and indices preserve coverage, including unconstrained roles.

## Path, mount and backing observations

The component walker uses retained nofollow descriptors and bounded readlink
expansion. It follows normal relative/absolute link semantics and processes
`link/..` through the actual link target. It never lexically cancels filesystem
components. Only an ordinary ENOENT permits a missing suffix; a dangling link,
missing/parent traversal, ENOTDIR, permission failure, device, FIFO or unsupported
procfs indirection is Indeterminate. Every traversed directory receives a kernel
search-access check using effective credentials. Nonstandard fsuid/fsgid
execution is conservatively Indeterminate so those checks cannot borrow
accessibility from another identity. No directory, file or test
write is created by the evaluator.

The kernel interfaces use `/proc/thread-self` so a calling thread cannot
borrow the process leader’s namespace or root evidence. Descriptor statx mount IDs (with bounded fdinfo fallback) select mount records.
Device and filesystem facts must agree with the exact mount ID; path containment
only supports that correlation. Separate mount and superblock options, escaped
roots/mountpoints, optional fields, nested mounts, bind roots, same-device mounts
and stacked mounts are retained. Duplicate IDs, malformed/truncated tables and
incoherent observations cannot Pass. A parent outside the visible namespace table
is permitted; it does not establish provenance.

| Backing | Host persistent-disk result |
| --- | --- |
| tmpfs, ramfs, recognized RAM disk/zram | Deny |
| ext2/3/4 or XFS with coherent supported disk leaves | Pass |
| Btrfs with ioctl filesystem identity, complete matching sysfs device set and supported leaves | Pass |
| Partitions, device-mapper and MD | Follow all supported dependencies; a volatile leaf denies |
| Loop devices | Indeterminate; loop-file origin resolution is deliberately unsupported |
| Network, FUSE, virtiofs, unknown devices/filesystems or hidden sysfs | Indeterminate |
| Host overlay/union | Indeterminate; the supported explicit `volatile` option denies |

Supported leaves initially include guest-visible SCSI `sd`, virtio `vd`, Xen
`xvd`, NVMe namespace and MMC block contracts through coherent kernel sysfs
topology and a recognized controller-driver contract. The initial driver registry
accepts virtio_blk, virtio_scsi, Xen vbd, NVMe, MMC and the ahci, ata_piix,
mpt3sas, megaraid_sas, usb-storage and uas SCSI controllers. scsi_debug,
RAM disk and zram evidence is volatile; unrecognized transports are Indeterminate.
A virtual composite without proven dependencies is not a disk leaf.
Filesystem type alone never proves persistence: ext4 on a RAM disk denies.
This workstation's virtiofs may not expose enough evidence to Pass. Virtual disks
are evaluated within the guest storage model; this does not establish hypervisor
persistence, hardware power-loss guarantees, future free space or remote durability.

## Protected v5 provisioning

V1-v4 loading and fingerprints retain their existing behavior. Host evaluation
works with older configurations. The container exception requires schema v5 or v6,
which retains all v4 delivery/gate/delegation requirements and adds exactly one
protected `storage_execution` assertion. The
[non-live v5 template](../memory-hooks-v5.toml.example) intentionally contains
invalid provisioning placeholders and cannot be activated as written.

The operator must independently establish an isolated Woodpecker container and
attest its canonical binding identity and project, current boot UUID,
mount-namespace device/inode, process-root device/inode, and a bounded exact set
of container-local mounts. Each mount declaration pins ID, filesystem root,
major/minor, mountpoint, mountpoint object device/inode, filesystem and explicit
`container-local` provenance. For Git bindings, `binding_root` is the canonical
Git common directory. A profile, `/tmp` string, namespace difference from PID 1,
environment variable, .dockerenv, cgroup name, or filesystem type is insufficient.

All declarations live in the existing protected, workspace-external configuration
path. The normalized v5 fingerprint includes every pin and assertion, with mounts
sorted by ID. Installation/configuration matching therefore requires normal
operator activation. Never copy runtime pins to a fresh container, reboot or
recreated namespace: re-establish isolation and provenance, reprovision the
protected configuration, and activate normally. There is no automatic adoption,
repository-side authority file, environment switch or namespace entry.

Mount IDs can be reused and namespace IDs are not durable credentials. Kernel
mountinfo cannot establish whether a mount was imported from a host. The protected
operator assertion supplies that provenance; a visible host/shared bind is not
container-local solely because it appears under `/tmp`. Each nested mount needs
its own exact matching declaration. Requested and resolved paths must both match
`/tmp/target` for TargetDirectory, or component-wise containment for BuildOutput.
The root and actual candidate backing are checked independently. An escaping
link, alias outside the root or `/tmp/target-other` cannot gain the exception.

This is a managed provisioning assertion, not kernel self-attestation. It relies
on the existing protected launcher/configuration and same-UID limits: an actor
who can rewrite the operator's protected files can change assertions. Raw pins,
paths, mount source/options and sysfs names remain private, without derived Debug.

## Bounds, races and audit handoff

Fixed limits: 64 unique candidate indices (0..63), 4096 bytes per candidate/link
and complete resolved path, 40 link expansions, 256 path steps, mountinfo at
1 MiB/4096 rows/16 KiB per row, 80 fields per mount row with each field bounded
to 4096 bytes, 16 backing edges deep and 128 total backing nodes per evaluation.
The shared pack limit remains 32 policies; registry expansion is capped at 64
requirements. Overall provider operations are capped at 65,536, and a cooperative
five-second budget starts at scope observation. Each provider boundary checks
the budget. An already blocked filesystem syscall is not interrupted by that
budget. The supervised process boundary supplies hard cancellation when #90
wires this library into pre-tool. Async callers must use `spawn_blocking` and
must reject late completion for an expired request.

Before returning, the evaluator reopens/rechecks path edges and link contents,
missing-prefix absence, namespace/root/boot identity, the mount table and observed
backing topology, including the Btrfs device set. Handles remain alive throughout
evaluation. Detected changes yield Indeterminate without retries. A later mount,
rename, link or device change can still invalidate the result: this library is
not a filesystem sandbox or race-free execution authorization.

The version-one report contains allowlisted outcomes/reasons, numeric errno,
candidate role/index, policy UUID/key/revision/project hash, validated pack digest,
scope digest, and domain-separated hashes of complete bounded requested/resolved
OS bytes and observed identities/backing topology. Guessable path hashes are not secrecy
guarantees. No policy prose, arbitrary values, environment, credentials, tool
payloads, OS error messages, mount source/options or device names are serialized.

Reports are capped at 128 KiB. Overflow produces a compact Indeterminate report
that retains decisive candidate/policy identity, complete-set counts (including
denials), and digests of every assessment and evidence item. No failing assessment
is silently dropped to manufacture a Pass. Candidate ordering is by numeric index;
policy ordering follows the authoritative pack and structured key ordering.

`Report::audit_projection` returns version-one aggregate counts, decisive safe
reason/candidate, policy UUID/revision with the remaining identity hashed, and
complete assessment/evidence digests. Fixtures verify the projection with existing
identity/delegation fields fits the 2-KiB record envelope. The managed consumer
validates and persists it in audit v3, bound to the exact checked claim, mandatory
pack, execution scope and bounded operation/provenance digest. Audit overflow or
corruption denies; old v1/v2 records remain validated without rewriting.

Tests use exact structured conversion-manifest policies, scripted path/kernel and
sysfs providers, conservative unsupported cases, controlled races, redaction and
bounds fixtures. Real probes read existing directories, execution descriptors,
mountinfo and backing only; unavailable live tmpfs/disk/container proofs are
explicitly skipped. No storage test runs a candidate command or Rust build,
creates privileged mounts, provisions a container or activates a live installation.

Kernel references: [proc mountinfo/fdinfo](https://www.kernel.org/doc/html/v6.5/filesystems/proc.html),
[OverlayFS](https://docs.kernel.org/filesystems/overlayfs.html), and
[Btrfs UAPI](https://github.com/torvalds/linux/blob/master/include/uapi/linux/btrfs.h).

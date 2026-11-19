# ADR-198: Shared Filesystem Ancestor Link Policy

- **Status**: Proposed (2026-10-06)
- **Date**: 2026-10-06
- **Depends on**: [ADR-080](ADR-080-session-pack-oss-storage-mechanism.md),
  [ADR-091](ADR-091-wal-snapshot-lifetime.md)
- **Addresses**: [#3959](https://github.com/ohdearquant/khive/issues/3959)

## Context and disposition

The session mirror, ANN segment sidecar and WAL-pin sidecar already use the same
descriptor-relative directory walker. Their private ancestor-link rules disagree about
ownership, writable parents, ACLs, identity checks and the link budget. Sharing the walk
does not resolve those differences.

This decision supplies one ancestor policy for the three consumers. It supplements the
accepted final-object boundaries in ADR-080 and ADR-091; it does not supersede either ADR
or change their status. A final mirror root, segment directory or WAL-pin sidecar remains
subject to its existing no-follow validation. WAL-pin sidecar mode, current-user ownership,
entry validation and descriptor-relative mutations remain unchanged. ADR-091's 2026-07-19
sidecar boundary says parent components must resolve without traversing a symlink at open
time; this decision reads that sentence through ADR-091's 2026-07-20 clarification, under
which identity binds from the database's containing directory downward and higher ancestry
is trusted platform layout. The WAL-pin acceptance of trusted ancestor links rests on that
reading, which does not require a private root-only policy in each consumer.

## Decision

`khive_fs::directory_walk::AncestorLinkPolicy` implements the existing `LinkPolicy` hooks.
It captures the process's effective uid when constructed. The generic walker, its
`LinkContext`, descriptor retention, raw-name handling and target splicing do not change.

An ancestor link may be followed only when all applicable conditions hold:

1. Root or the captured effective uid owns the link.
2. Root or that uid owns the pinned directory containing the link.
3. That directory has no group/other write bit, or has the sticky bit.
4. On macOS, its descriptor-derived extended ACL contains no entry that grants a right.
   No ACL, DENY-only entries and ALLOW entries with an empty permission mask do not grant
   rights. Any nonzero ALLOW permission mask refuses, including permissions outside a
   narrower integer mask. An unknown tag, uninspectable ACL or failed witness refuses.
   This is an entry-level grant restriction, not an effective-access calculation: principal
   matching, preceding DENY entries and inheritance flags do not excuse a granting entry.
5. After reading the target, the link's device, inode, mode and uid equal the original
   no-follow metadata from the same pinned parent/name. A failed re-inspection also refuses.
6. The three consumers pass the common `ANCESTOR_LINK_BUDGET = 8` to the shared walk.
   The ninth otherwise permitted link ends with the existing `BudgetExhausted` error.
7. A link that names the final target is refused. `AncestorWalkEndpoint::FinalTarget` is
   used by session and Vamana. WAL-pin walks only its target's parent, so it uses
   `TargetParent`: the last component of that walk is still an ancestor. Its separate
   final-name no-follow open remains mandatory.

The parent metadata and ACL witnesses come from the pinned descriptor, not from reopening
its pathname. On non-macOS Unix targets this decision adds no ACL interpretation. Windows
reparse-point checks and portable fallback behavior are unchanged.

The session adapter additionally retains its existing target-length check and final-root
kernel-error conversion. Adapters may retain context and error transport, but may not keep
independent ownership, permissions, ACL, identity or budget rules.

## Refusals and public surface

The shared module exports `AncestorWalkEndpoint`, `AncestorLinkPolicy`,
`ANCESTOR_LINK_BUDGET`, `AncestorLinkCondition` and `AncestorLinkRefusal`. Refusal conditions
are `FinalComponent`, `LinkOwner`, `ParentOwner`, `ParentPermissions`, `ParentAclGrant`,
`ParentAclWitness`, `ParentMetadata` and `LinkChanged`. Display names are respectively
`final_component`, `link_owner`, `parent_owner`, `parent_permissions`, `parent_acl_grant`,
`parent_acl_witness`, `parent_metadata` and `link_changed`.

An `io::Error` carries the typed refusal, including its component, original link uid and
available parent uid/mode. Witness failures retain their underlying I/O error as a source.
The generic walker returns policy errors unchanged. Vamana retains its public
`ExternalIdsWriteError` variant shapes and carries a shared refusal through its existing
`Io` variant; it must not replace the refusal with a generic walk error. Existing NUL,
system-call and budget contexts remain specific.

## macOS witness implementation

The extended ACL is read using `acl_get_fd_np` on the pinned parent. An owned guard releases
the ACL on every exit. `acl_valid` checks the ACL handle, not the contents of every entry;
each iterator, tag and full 64-bit permission-mask result still needs inspection.

Darwin's iterator returns zero for an entry and minus one with `EINVAL` when fixed
FIRST/NEXT selectors exhaust a validated, privately owned ACL. It does not use the POSIX
one/zero entry/end convention. Unexpected statuses, null entries, unknown tags, failed
tag/mask calls and an oversized sequence refuse. The no-ACL case is the documented
`ENOENT` result; another retrieval error cannot become a passing witness.

The primary interface and iterator behavior are pinned in
[Apple's ACL header](https://github.com/apple-oss-distributions/Libc/blob/71bbe350ab79eef58113991d817ccc6165061a64/include/sys/acl.h)
and [iterator implementation](https://github.com/apple-oss-distributions/Libc/blob/71bbe350ab79eef58113991d817ccc6165061a64/posix1e/acl_entry.c).
These source definitions do not substitute for execution on a supported macOS SDK/runtime.

## Deliberate behavior changes

| Consumer | Newly accepted cases                                              | Newly refused cases                                                                                        |
| -------- | ----------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| Session  | Trusted effective-user links/parents; ACLs with no granting entry | No new final-target acceptance; existing ownership, unsafe-parent, witness and replacement refusals remain |
| WAL-pin  | Trusted effective-user ancestor links                             | Foreign/unsafe/ACL-granting parents and changed links previously unchecked                                 |
| Vamana   | Sticky protected ancestor parents                                 | macOS granting/uninspectable ACLs, changed links and the ninth through fortieth link                       |

The corresponding root-only, private-predicate and forty/forty-one test expectations are
replaced or updated explicitly. Final-target and final-sidecar behavior remains unchanged.

## Acceptance and adversarial evidence

The private table runs the same evaluator used by the production policy. It pairs newly
allowed effective-user ownership, sticky parents and non-grant ACLs with adjacent foreign-owner,
nonsticky-parent and granting-ACL refusals. A separate foreign-link and foreign-parent
must-DENY control is mandatory. It must deny on the actual driver; an empty or always-ALLOW
instrument cannot establish this policy.

Real shared walks cover accepted versus refused ancestry, exact eight/nine-link boundaries,
final-target versus parent-walk endpoints and a deterministic replacement between the hooks.
Each of the three production caller entrypoints has a trusted-current-user acceptance and
unsafe-parent refusal carrying the shared condition. Vamana additionally proves refusal
precedes sidecar writes. Existing final-object, source-leaf, target-length, raw-name and
WAL-pin heartbeat-target checks remain.

macOS execution must distinguish no ACL, DENY-only/non-grant ACL, nonzero ALLOW masks and
failed witnesses while mode bits remain suitable. Removing ownership, grant detection or
identity checks must break their cold/must-DENY cases; reverting sticky or non-grant acceptance
must break the corresponding hot cases. The existing cycle budget-decrement control remains.

The newly allowed set is nonempty, so adversarial execution evidence is required before this
Proposed decision can license dependent landing. That evidence must include the must-DENY
result and adequate hot/cold coverage, with platform-specific results kept distinct. Source
inspection and an unexecuted suite do not constitute acceptance. No statistical efficacy,
sampling or measured performance claim is made by this decision.

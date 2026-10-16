# ADR-203: khive Apps on the Base: Manifest, Lifecycle Verbs, Store Classes, Registration and Supervision

**Status**: proposed\
**Date**: 2026-10-16\
**Depends on**: ADR-023, ADR-027 (Amendment 4), ADR-028 (A4, A5), ADR-029, ADR-067, ADR-111, ADR-119, ADR-127, ADR-129 (Stage 1b), ADR-143, ADR-161, ADR-185, ADR-002

## Founder direction this record serves

1. 2026-10-15: bring a private kernel design corpus into khive-oss as the base; an installable, stoppable unit with its own storage and cross-app links.
2. 2026-10-15, later the same day: the unit is installed and uninstalled, and the record defines what a khive app is in khive-oss terms.
3. 2026-10-16: the embedding and inference library is a system service and a library, not an app.

Every decision below is checked against these three lines first. Where a decision departs from them the sentence says so, and the departure is the founder's call, never this record's.

## Context

khive-oss is the base. Today a pack is a Rust crate linked at build time and discovered through self-registration (ADR-027); the running binary loads a configured subset of the twenty pack crates into one process with one dispatch table, and packs compose in-process (ADR-028 §9). Each pack can own a backend through `[packs.<name>] backend` (ADR-028 A4), and the base can read entities and notes across pack backends (ADR-028 A5). Long-running work inside the daemon is a supervised component in-process (ADR-119), which names "run every component as a subprocess" the mandatory fallback once in-process isolation is shown insufficient; the events lane and the brain state already left the domain process as subcommands of the same binary (ADR-170, ADR-171). Authorization is an authenticated actor with store-held grants (ADR-127, ADR-143) behind a gate that fails closed (ADR-129); the concrete capability gate of Stage 1b is not yet on `main`. Installable distributions have two profiles, data-only now and WebAssembly later (ADR-027 Amendment 4); native dynamic loading stays rejected.

What does not exist: a word for the unit the founder wants to install, stop and uninstall; a manifest that declares what such a unit owns and asks for; a rule for where its rows live and what survives its removal; a registration path for a unit that is not linked into the binary; and a supervisor for a unit that runs as its own process. The first three named candidates are a visual retrieval pack already linked into the binary (ADR-148), a medical records application that preserves original images as its authoritative record, and a finance application not yet started.

A design corpus written for a private successor kernel holds the fuller model of these parts: an app as manifest plus ontology slice plus skills, store classes with projection across stores, installation-bound grants, a process lifecycle with decider-tagged terminal events, and a pack as a process with a declared verb surface linked at registration time. Those records are cited here by their key; this record ports their decisions into khive-oss terms where khive-oss does not already hold an equivalent, and names which parts khive-oss already decided.

## Decision

### D1: The unit is a khive app; the verbs are install, start, stop, uninstall and purge

A **khive app** is a bundle with a manifest (D2), zero or more verbs, an ontology slice (entity kinds, note kinds, edge endpoint rules it contributes), skills and resources as typed content, optionally declared corpus stores, and a residency the host validates (D3). The vocabulary follows line 2: "extension" and "killed" are not used. The lifecycle verbs are:

- `install`: verify the manifest signature, present the requested rights and the ontology slice, record the grant durably bound to this installation (D7), register the slice.
- `start`: admit a process app under the supervisor contract (D4); a no-op with a stated reason for an app with no process.
- `stop`: no new app execution; admitted work drains or is terminated under D4; storage and links stay; cross-app links to its rows go dormant, never dangling.
- `uninstall`: revoke this installation's grants, deregister the slice from future registration, demote provenance on the app's rows; delete nothing.
- `purge`: drop the app's declared corpus stores as a unit; projections of dropped rows survive marked origin-dropped; cascade is opt-in per act; primary rows and retained originals are never swept by purge (D6).

Core packs linked into the binary keep their shape under ADR-027 and are not apps. The visual retrieval pack becomes the first app by declaring a manifest; its code does not move until D3 says it must.

### D2: One manifest, declared not discovered

The manifest is the app's whole public identity. Nothing about an app is learned by importing its crate. It carries:

- identity: name, version, signer;
- the verb inventory, each handler with its rights class, one of `Read`, `Create`, `Write`, `Delete`, `Send`, declared explicitly and never derived from the handler's category (the `Commissive` category covers both create-shaped and send-shaped verbs, so it cannot stand in for the rights class);
- ontology contributions: entity kinds, note kinds, edge endpoint rules, under ADR-002's pair restrictions and ADR-023's declarative pack format;
- row-type declarations: every row type the app writes names its lifecycle class, `primary` or `corpus`, and for `corpus` the declared store it lives in (D6). A row type without a declaration refuses installation. This departs from an earlier proposal that an undeclared type is app-owned: that default would let a manifest move user-authored rows into the disposable class by omission, which breaks the ownership guarantee of D6;
- requested capability classes (D8);
- skills and resources, as typed content carrying no authority;
- a residency request, `none`, `linked` or `process`, that the host validates against the payload (D3); the request never decides by itself.

Two manifests that do not conflict compose without knowing of each other; a conflict on a verb name or a type claim refuses the later registration naming both owners. The manifest type lands in `khive-types` beside today's `HandlerDef`, `EdgeEndpointRule` and `NoteKindSpec`, which already carry the per-verb and per-kind parts of it.

### D3: Residency follows the executable shape, never the trust label alone

An app's identity and its execution residency are separate. Three residencies exist, and the host assigns one by reading the payload:

1. **No process.** A data-only app (ADR-027 Amendment 4, profile 1): record kinds with closed schemas, bounded validation rules, verb parameter schemas, skills and resources. The host interprets all of it. Enabling such an app consumes no process slot; its status reports "no worker" rather than a fabricated process identity.
2. **Linked module.** Code already linked into the binary (today's pack crates) runs in-process only while it has a complete quiesce contract: every thread, socket and timer it owns is enumerable and drains on `stop` within a declared bound. Until a crate demonstrates that contract, it is not eligible for `stop` as a linked module and runs as a process if it needs independent stop at all. Nothing here reopens native dynamic loading.
3. **Supervised process.** Any independently installed native executable, any app with autonomous or unbounded work, and any app whose memory the owner must be able to reclaim runs as a child process of the daemon under D4. This is the subprocess fallback ADR-119 reserved, made concrete for apps. First-party authorship does not waive the criteria.

Authority never depends on residency: the gate's decision is identical for a linked and a separated caller, and nothing about attribution or audit may read residency. Core kg, memory and brain composition stays in-process. The process boundary is not by itself a sandbox: an escaped descendant is uncontained and that limit is stated, so untrusted execution remains the separate decision of ADR-109.

This departs from the earlier lean "an app is a process under a supervisor" for every app: a passive app would then pay a process lifecycle and an IPC crossing per call for no isolation it needs, and store separation never required process separation in the first place (each physical store has one writer and its own queue inside one daemon, ADR-028 A4, ADR-067).

### D4: The supervisor contract for process apps

The daemon owns the supervisor. For a process app:

- **Admission at start.** `start` reads the kernel's memory pressure level and the load average and refuses above a configured threshold with a typed refusal; refusals are a closed set: `pressure`, `load`, `not_installed`, `stopped`, `already_running`.
- **Identity.** A running app is identified by pid plus process start time, never by pid alone; a pid that fails the start-time check is not the app.
- **Environment.** Every executable path in the child's environment is absolute; the child inherits no shell profile.
- **Exit contract.** Exit codes are a closed vocabulary the app documents; the terminal event names its decider (the app's own exit, or the daemon's stop, deadline, lease lapse or revocation) and carries that decider's own code, as a tagged sum so a decider-blind read does not compile.
- **Stop.** Graceful-then-forced: a stop signal, a declared grace window, then force. Each process group the app registers at start is signalled beside the main process; a child that escapes its registered group is uncontained, and status says so rather than claiming containment.
- **Liveness.** A renewable lease; expiry is evidence for the supervisor, never execution by itself.
- **Restart.** A bounded restart policy declared in the manifest; the supervisor never restarts past the bound without an owner act.
- **Status.** Separates enabled, ready, active calls, worker liveness and residual or unknown resource use; cost attribution names shared base work and shared memory as shared rather than summing resident set size as if it were app-exclusive.
- **Crash of the supervisor itself.** Desired state is durable; on restart the supervisor adopts a child whose pid plus start time matches its record and terminates one that does not; duplicate starts are refused by an occurrence key.

An orchestration runtime that spawns agent legs is a client of this supervisor: a run that needs an app asks the daemon through a verb and records the app id, never the pid. Merging the two supervisors is not decided here.

### D5: Registration over the daemon wire is the link step

A process app registers over the existing daemon socket (ADR-185 rendezvous, one frame per connection, peer-uid admission) with a registration frame carrying its manifest; the version handshake of `khive-wire-protocol` is the versioned contract. The dispatch table is built from registrations and installed manifests; there is no hand-maintained map. A linked module registers at boot as today. MCP stays the outbound face of the base; apps do not speak MCP to the base.

The handshake proves frame admission, not operation semantics: D9 states what an app call's timeout means.

### D6: Store classes: primary state belongs to the shared substrate; corpora belong to the app

Two lifecycle classes, declared per row type (D2):

- **Primary.** Rows a person or an agent authors or retains through the app: notes, tasks, decisions, records the owner has saved or corrected. They live in the shared spine from the first write, typed by the app's ontology slice, attributed to the run that wrote them with the app's installation in the chain, evented like all writes, readable by any principal holding a grant without the app's code, and inside the backup set. They outlive stop, uninstall and purge. This is what makes the owner's data the owner's.
- **Corpus.** Bulk rows the app imports or derives and can re-acquire from a stated source: a repository code map, an image feature store, a vector index. They live in the store the manifest declares (ADR-028 `[packs.<name>] backend`), partitioned by corpus, droppable as a unit, outside the precious backup set.

Rules:

- Original bytes an app ingests (a photograph, a document) are retained as blob content (ADR-111, ADR-155) referenced from a primary row; "the app imported it" never makes an original disposable.
- A cross-app edge is a spine row, always; the corpus endpoint is promoted at edge-write time into a projection carrying the same id, kind, display properties and origin provenance; domain-internal edges promote nothing. The spine holds exactly the primary state plus the subset of each corpus the outside world has referenced.
- A projection says what it is: its source revision, whether the target is stopped, origin-dropped or temporarily unavailable; it never widens visibility beyond the source row's grants and never claims freshness the source cannot supply.
- A hidden side database is banned: state in undeclared files, invisible to grants and outside the one id space. A declared corpus store is substrate-governed and is not one.
- The class is a declaration checked at install, never a lever: an upgrade cannot reclassify an existing primary row as corpus; a manifest that tries refuses installation.
- The declared store's schema and decoder stay readable by the base after the app stops or is uninstalled.

This departs from the earlier reading of "each have their own storage" (line 1) as "all of an app's rows in its own store": the app's own store is its corpus store, and the owner's authored rows are the owner's regardless of which app wrote them. The founder's line names ownership of storage; it does not name ownership of the person's data by the app, and the second reading would make purge a data-destruction event.

### D7: Grants bind to an installation

An installation is the signer plus the manifest digest plus an install sequence number. Install records the grant against the installation; uninstall revokes that installation's grants and no other grant of the same signer; a reinstall is a new installation with new grants and revives nothing. A run carrying the app's skills records the installation among its sources. Stopping one installation fences new work for that installation only; a run using several installations keeps the others.

### D8: Capability is the base's one model, finished

Apps are authorized by ADR-127 grants held in the store (ADR-143), evaluated by the concrete capability gate of ADR-129 Stage 1b, with one sealed capability implementation in the base. No second policy evaluator sits beside it: two tables that must agree is one table too many. The rights class declared per handler (D2) is the vocabulary an app manifest requests and an installation grants, so laptop and multi-tenant deployments use one set of terms. Spawn attenuates and nothing amplifies (ADR-161's shape). The one implementation also owns custody of its sealing material, rotation with a grace window, and refusal of a tampered descriptor before any table read; those three are the parts the base does not have today and are acceptance criteria for the port.

### D9: An app call that times out has a stated outcome

Process exit and reply loss do not prove rollback. Each app verb declares which of its effects are atomic at the substrate (a committed mutation is either whole or absent), which outcomes are unknown after a timeout, and which retries deduplicate under an occurrence key. There is no distributed transaction across an app's corpus store and the spine; a verb that needs both writes the spine last and records the corpus write's id in it.

### D10: Identity across reinstall and purge

The same ontology slice with the same canonical keys rebinds corpus identity after a re-ingest; a different slice is a different id universe and nothing rebinds by design; migrating references between slices is an explicit crosswalk act, proposable by an agent, approved by the owner, never automatic. Reinstall restores no lost corpus content: an identical id with a different body is not restoration. The purge unit is the app's corpus partition; app code, installation, primary rows, projections, blobs, archives and backups are named separately in the purge verb's read-back so the owner sees the exact scope.

### D11: The embedding and inference library is not an app

Per line 3, the embedding library stays linked in-process and generative serving is a base-owned system service: the base starts and supervises it as a unit with the same process boundary as D4 for memory return and fault isolation, but only stop and restart apply, and its weights cache is base state. Its unit definition rides the library's own record, not this one.

### D12: Cross-app links live in the base

Links between apps are typed edges with stable ids in the spine, with projections (D6), so the base answers "what points at this" when the target is stopped. Whether the relation set needs new app-to-app kinds beyond `annotates`, `links_to` and the existing relations is a separate amendment to ADR-002, decided when the first app pair needs one; crossing an app boundary does not by itself make a new relation kind necessary.

## Acceptance

Each decision names the artifact that proves it landed. Nothing below is claimed as done.

- D1, D2: the manifest type in `khive-types`; an install fixture refusing a manifest with an undeclared row type and one with a verb name already owned, naming both owners; a conformance fixture running install, author, link, stop, uninstall, purge and reinstall with an independent reader and an unlinked control row, asserting every primary row readable after each step without the app's code.
- D3: a passive app enabled with no process and a composed read through it; a control showing the same behaviour, authority and stop semantics with the process façade removed; a residency-eligibility fixture refusing `linked` for a crate without a quiesce contract.
- D4: a supervisor fixture per refusal; pid plus start-time identity rejecting a recycled pid; a terminal-event fixture pair, app-decided versus daemon-decided, arriving as distinct variants; a stop fixture with a registered process group and an escaped child, the escaped child reported uncontained; a supervisor-restart fixture adopting a matching child and terminating a mismatched one.
- D5: a registration frame fixture through the real handshake, and a refusal of a registration whose manifest conflicts.
- D6: a cross-app edge write minting the projection; a purge fixture asserting partition removal, projection survival marked origin-dropped, primary rows and blob references untouched; a schema control asserting no corpus store carries edge kinds beyond its declared set; a reclassification attempt refused at upgrade.
- D7, D10: an uninstall fixture asserting revocation of exactly this installation's grants and the rows' continued existence under a surviving grant; a reinstall fixture asserting no revived grant and corpus rebinding only for the same slice and keys.
- D8: Stage 1b landed with the three named parts; a tampered descriptor refused before any table read.
- D9: a timeout fixture per declared outcome class.
- D11: covered by the library's own record.

Measurements the acceptance owes before the first process app ships, each run on a build host and never on the owner's loaded machine: enable latency, idle wakeups and private memory of a passive app with and without a process façade; writer wait distributions for two stores in one host versus separate workers; extra crossings, bytes and latency of one app verb calling base kg and memory through the socket against the in-process path, with a core-only call as the negative control; private memory returned after stopping a process app against draining a linked one.

## Rationale

- Separating identity from residency keeps the founder's install, stop and uninstall semantics for every app while refusing to pay a process for an app that has no work of its own; the process is the answer where the owner must be able to reclaim memory or interrupt work that makes no further call into the base.
- Primary rows in the shared substrate are what make stop and purge safe acts: the only data an app can take with it is data it can get back.
- One capability model with declared rights classes is what lets the same manifest install on a laptop and in a multi-tenant deployment.
- Registration as the link step is what makes a third-party binary installable without a rebuild, and the handshake is already versioned.

## Alternatives considered

- **Every app is a process.** Rejected as the universal default: lock sharing argues for separate stores, not separate processes; a passive app would pay process lifecycle and IPC per call for nothing. Kept as the required residency for executables and unbounded work (D3).
- **Every app is an in-process module with lifecycle verbs.** Rejected: a stopped linked module still occupies the address space; a crash takes the daemon down; new native code cannot be installed without dynamic loading, which stays rejected; "stopped" is a claim a module with its own threads cannot make without a quiesce contract.
- **Every app is a data-only distribution.** Rejected as universal: the visual retrieval pack and the medical records application need algorithms the host does not interpret. Kept as the first residency (D3).
- **Classify rows by who wrote them.** Rejected as the admission rule: a seat bulk-importing a corpus is still a corpus, and an app-produced record the owner retains is still primary; the run's identity is evidence, not a retention policy (D6 uses declared classes checked at install).
- **All rows in the app's store, the spine holds only edges and a resolver.** Rejected: every global query would need a per-app read contract and the projection contract would be dropped for no stated benefit.
- **Undeclared row types default to app-owned.** Rejected (D2): it defeats the ownership guarantee by omission.
- **A policy-language evaluator beside the capability gate.** Rejected (D8): two tables that must agree.
- **A second supervisor in the orchestration runtime.** Not decided; the daemon's supervisor comes first and the runtime is its client (D4).

## Consequences

- Positive: one word and five verbs for the thing the founder asked for; the owner's rows survive every lifecycle act; a third-party binary can be installed without a rebuild; cost and liveness of an app are observable per installation.
- Negative: two residency implementations (linked and process) cost more than one; the supervisor is a new daemon subsystem with its own failure modes; primary rows share the spine's writer, so the measurements in Acceptance decide whether a physical partition below the logical substrate is needed before the first heavy app.
- Neutral: the private design corpus becomes donor material for this record and the capability port; what happens to that corpus as a whole is the founder's decision and nothing here depends on it.

## Appendix A: inventory of the private design corpus against khive-oss

See the inventory table shipped beside this record; each record of that corpus is listed with its disposition (as written, rewritten, already in khive-oss, superseded, out of scope) and the khive-oss record it lands against. Four records supply this record's text (the app abstraction, the pack manifest, the storage substrate's classes and projections, the usage model's "apps are granted to processes"); nine supply slices; the rest either already exist here or stay outside the base.

# ADR-185: Daemon Rendezvous Ownership — a client must not take the socket a supervisor is for

- **Status**: Accepted (2026-09-22)
- **Date**: 2026-09-13
- **Depends on**: [ADR-049](ADR-049-khived-daemon.md) (the warm-state daemon, the thin client, and
  client auto-spawn)
- **Relates to**: [ADR-096](ADR-096-warm-daemon-per-request-identity.md) (the daemon's configuration decides
  the identity every call through it resolves against)

## Context

ADR-049 §2 gives the thin client one job beyond dispatch: if no responsive socket exists on the first
request, the client spawns the daemon itself. That was written for a single-user workstation where
the daemon has no other way to start, and there it is exactly right — the first call after a reboot
pays a startup cost and everything after it is warm.

A deployment that puts the daemon under a process supervisor has a second starter, and ADR-049 does
not say what happens when both are live. What happens is a race for the rendezvous, and the client
wins it structurally:

- The client spawns **on demand**, at the instant of a call, from whatever process happens to call
  first.
- The supervisor starts **on a schedule**: at load, and after a failed start on a throttle interval.
- The daemon's own startup contract (ADR-049, and the refusal path in the runtime's daemon module)
  is _first writer wins_: once the PID file names a live process that answers on the socket, every
  later starter is refused with "a khived instance is already serving this socket" and exits
  non-zero.

That refusal is correct in isolation and is what makes the race one-sided. The supervisor's job exits
non-zero against the incumbent, is rescheduled, exits non-zero again, and never owns the daemon it
exists to supervise.

### What was measured

On a supervised restart of a deployment carrying both starters:

1. Every client process known at the time was enumerated and frozen, the incumbent daemon was
   stopped, the new binary installed, and the supervisor kickstarted **first**, with the clients
   still frozen. This ordering is the obvious remedy and it is not sufficient.
2. 69 seconds later the supervisor's job still had no pid, and the socket was already held by a
   daemon carrying the configuration of a _different_ client — one that started after the
   enumeration and was therefore never frozen.
3. The supervised job's last exit code was 1, its state "spawn scheduled": it had tried, been
   refused by the incumbent, and gone back to waiting.
4. Stopping that daemon by hand did not help. Another client won the free socket within 45 seconds,
   and the supervisor lost again.

The freeze can only cover the clients that exist when it enumerates them. In a deployment where
clients are started by anything asynchronous, the set is open, so no amount of ordering closes it.

### What it costs

- **The fleet daemon's configuration is whichever client called first.** Every call routed through
  that daemon resolves identity and backend paths against that configuration (ADR-096). A client
  whose own configuration disagrees is served a mismatch and falls back to running in-process,
  silently, which is the failure mode hardest to notice because everything still answers.
- **There is no supervision.** No restart policy, no keep-alive, no operator control of the process
  that holds the warm state for every client on the machine.
- **An operator has no way to say which configuration the daemon should hold.** The supervisor's
  declaration is the natural place to say it, and the supervisor never gets to run.

## Decision

**Where a supervisor for the daemon is declared, the client must not auto-spawn.** Auto-spawn stays
the default and stays unchanged for the single-starter case ADR-049 describes.

This is the recommended shape rather than socket activation, for one reason: it is the only one that
works on a deployment the platform does not help. Socket activation removes the race where the
platform offers it and should be taken there, but a declaration the client reads is what makes the
answer the same on every platform, and it is also the only shape that gives an operator somewhere to
say which configuration the daemon should hold.

Concretely:

1. The runtime gains an explicit way to declare that daemon startup is owned elsewhere. The
   declaration lives in the daemon's configuration file, the same file the supervisor's own
   invocation names, so an operator states it once in the place that already describes the process.
   An environment variable overrides it for a single client. It is read at the point the client
   decides to spawn, not at process start, so a deployment can turn it on without restarting every
   client.
2. Where the file and the environment disagree, the client takes the stricter of the two, refuses,
   and names both values. Silently preferring one would make the failure above reachable again
   through a disagreement that is invisible from either side on its own.
3. With a declaration in force and no responsive socket, the client **refuses the request** rather
   than spawning, and the refusal names the owner and says the daemon is expected to be started by
   it. A refusal is correct here: a request that would otherwise be served by a daemon anchored on
   the caller's own configuration is a request whose result cannot be trusted by anyone else.
4. Before refusing, the client waits a bounded interval, at most 10 seconds, for the owner's daemon
   to answer on the socket. Inside that wait the caller sees "the daemon is starting"; after it, "not
   yours to start". The two are distinguishable in kind, so a caller can retry the first and must not
   retry the second in a loop, and the wait is what lets the first call after a supervised start
   succeed rather than refuse once.

### Alternatives considered

- **Socket activation**: the supervisor binds the socket and hands it to the daemon, so no client can
  take it. This is the strongest form and removes the race rather than refusing it, but it binds the
  design to platforms that offer it and changes the daemon's own bind path. Worth doing where
  available; it does not remove the need for (1), because a deployment without socket activation
  still needs a way to say "not yours to start".
- **The auto-spawned daemon adopts a canonical configuration** rather than the caller's. This fixes
  the configuration half and leaves the supervision half broken: the daemon still runs unsupervised,
  and the supervisor still exits non-zero forever.
- **Leave it to operators**: document that clients must be started with auto-spawn disabled. This is
  what a deployment can do today with an environment variable per client, and it is exactly the kind
  of invariant that holds until one client is started without it — which is the state that produced
  the measurement above.

## Consequences

- A deployment that declares an owner gets a deterministic answer to "whose configuration is the
  daemon holding": the owner's.
- A client in such a deployment can fail where it used to succeed. That is the point, and it is why
  the refusal must name the owner: the alternative is succeeding against a daemon nobody chose.
- The single-starter case is untouched. No configuration, no declaration, no change.
- The existing first-writer-wins refusal stays as it is. It is not the defect; it is the mechanism
  that made the defect one-sided, and it is still what protects a running daemon from a second one.

## Acceptance

The arm that decides whether this record is implemented, stated before anything is built, because it
is the one the obvious remedy already failed:

- **A client started while no daemon is serving and an owner is declared must not take the
  rendezvous.** The test starts from no socket and no PID file, declares an owner, starts a client,
  and asserts the client refused: no socket appeared, no PID file appeared, no daemon process exists,
  and the refusal names the owner. Ordering is not part of the arm — the client is started at the
  moment that would have won under today's behaviour.
- The same arm with no owner declared is the control, and it must produce the opposite result: the
  client spawns and serves, exactly as ADR-049 specifies. Without that control the first assertion is
  satisfied by any broken client.
- A third arm covers the incumbent case that already works and must keep working: with a daemon
  already serving, a client neither spawns nor refuses, it connects.
- A fourth arm covers a client whose configuration file and environment name different owners: it
  must refuse and name both values rather than pick one. The agreeing case is its control and must
  not refuse.
- The refusal's kind is asserted, not just its text, since a caller has to tell "not yours to start"
  from "starting, try again" without parsing prose. The bounded wait is asserted on both sides of
  its boundary: inside the window the kind is "starting", past it the kind is "not yours to start".

## Amendment 1 (2026-09-22): the declaration is a launcher-written marker, and suppression is bounded

This amendment replaces Decision items 1 and 2 and bounds items 3 and 4.

### What exists since the record was proposed

A client-side reader has shipped. The client reads a marker file at the rendezvous (by default
`~/.khive/khived.supervisor`; `KHIVE_SUPERVISOR_MARKER` overrides the path) that holds a job label and a
pid. With no marker the client follows ADR-049. With a marker and no responsive socket it never
spawns: a live pid earns the bounded wait and then a refusal naming the job, and a dead pid refuses at
once. Nothing defines who writes the marker, so the only writer today is a temporary one in the local
install target. Three questions were open: what activates supervision, who publishes the claim and
when, and who owns the marker across daemon death, restart backoff and deliberate stops.

### Decisions

1. **Activation is the marker, written by the launcher.** The declaration of item 1 is the marker file
   itself, not a configuration key. The process the supervisor starts (a wrapper, or the supervisor's
   own pre-start hook) writes the marker before it execs the daemon. The configuration-file
   declaration and the environment override of items 1 and 2 are withdrawn, and
   `KHIVE_SUPERVISOR_MARKER` stays a path override for tests only. The writer has to be the launcher
   because the race is lost before the daemon runs: a writer inside the daemon publishes the claim
   after a client may already hold the socket.
2. **Format.** Three lines: the supervisor's job label; the launcher's pid, which the daemon keeps
   across exec; and the supervisor's restart interval in whole seconds (launchd `ThrottleInterval`,
   systemd `RestartSec`). The launcher writes a temporary file in the same directory and renames it
   over the marker, so a reader never sees a partial marker. A marker without the third line reads as
   a 10-second interval, the launchd default. An unreadable marker still suppresses, as shipped.
3. **Ownership.**
   - The daemon never writes or removes the marker.
   - A launcher overwrites a marker that carries its own job label, since that is a restart of itself.
     It never touches a marker that carries another label. Two supervisors declared for one
     rendezvous is a configuration error, and the second launcher refuses to start.
   - The launcher, or the deployment's stop procedure, removes the marker on every deliberate stop.
     That includes the launcher's own configuration refusal: a launcher that exits successfully so
     that the supervisor does not restart it removes the marker first. Once it is removed, clients
     follow ADR-049 unchanged.
   - An operator removes a marker by hand only when decommissioning the supervisor.
4. **Suppression is bounded by the restart interval, for a live pid and a dead pid alike.** A client
   that finds a marker and no responsive socket waits, re-reading the socket and the marker, for at
   most N times the marker's interval from the start of the request, with N = 3.
   - A socket that answers ends the wait, and the client connects.
   - A marker that disappears ends the wait, and the client follows ADR-049.
   - A caller deadline that expires first returns the retryable "starting" kind of item 4.
   - Past the bound, with the marker still present and no socket, the client may start the daemon
     itself. It logs a degradation naming the job, the pid and whether that pid was alive, the
     marker's age and the time waited ("supervisor present, daemon absent"). It never starts one
     silently.

   The 10-second wait of item 4 is too short for a dead pid. A supervisor restarts a crashed daemon
   one restart interval after the crash, and launchd's default interval is also 10 seconds, so a
   client bounded at 10 seconds reaches the free socket at the same moment as the restart. That is
   the race measured above. Three intervals cover the restart and the daemon's own startup with a
   margin. A deployment whose daemon takes longer than two intervals to bind raises the interval it
   writes into the marker.

   The bound also covers two failures that a pid check cannot see. In a crash loop every restart
   rewrites the marker, so its age never grows. A reused pid that now belongs to an unrelated process
   reads as alive indefinitely. Without the bound, either one suppresses every client for as long as
   it lasts, behind a supervisor that looks healthy.

   After this amendment the spawn path returns no permanent "not yours to start" refusal. A caller
   either is served or receives the retryable "starting" kind.

### Consequences of the amendment

- A request that arrives while the supervised daemon is down waits up to three restart intervals (30
  seconds under launchd's default) and is then served. Today it is refused at once when the pid is
  dead, or after 10 seconds when the pid is alive.
- A supervisor in a crash loop can no longer suppress every client indefinitely. The cost is that a
  client-started daemon can then hold the socket, and the supervisor's next start is refused by that
  incumbent, which is the original failure. It can only happen after the bound, and a log line names it.
- No configuration key is added.

### Acceptance changes

The arms above stand, with "an owner is declared" read as "a marker is present" and "not yours to
start" read as "past the bound, the client starts the daemon and logs the degradation". The arm for a
configuration file and environment that disagree is withdrawn along with that declaration. Added:

- **The launcher writes before exec.** Start from no socket, no PID file and no marker. Start the
  launcher and a client together, the client at the instant that wins under ADR-049. The supervised
  daemon holds the socket, the supervisor's pid equals the socket holder's pid, and exactly one daemon
  process exists.
- **A configuration refusal releases the rendezvous.** A launcher that refuses its configuration exits
  successfully and leaves no marker, and a client then spawns and serves per ADR-049. Control: the same
  refusal with the marker left in place makes the client wait out the bound.
- **A crash loop is bounded.** A launcher whose daemon exits non-zero before binding is restarted on a
  short interval. A client waits no longer than three intervals, then starts the daemon and emits the
  degradation. Control: a daemon that binds within one interval is connected to, with no degradation.
- **The respawn gap is covered.** A healthy supervised daemon is killed. A client that arrives before
  the supervisor's restart does not start a daemon; the restarted supervised daemon binds, and the
  client connects to it. On the real deployment, the supervised start-to-bind time is measured ten
  times, and every sample fits inside two intervals.
- **A foreign marker is left alone.** A launcher that finds a marker carrying another job label refuses
  to start and leaves the marker untouched.

## Amendment 2 (2026-09-24): the launcher replaces a client-started incumbent

This amendment adds Decision item 5 to Amendment 1, replaces one sentence of Amendment 1's consequences, and adds
acceptance arms. Everything else in Amendment 1 stands.

### Why ordering is not enough

Amendment 1's arm "The launcher writes before exec" starts the launcher and a client together, the client at the
instant that wins under ADR-049, and requires the supervised daemon to end up holding the socket. Serializing the
client's marker read against the launcher's publication decides the order of the two, but not in the launcher's
favour. When the client's decision comes first there is no marker yet, so the client spawns under ADR-049 and its
daemon binds. The launcher then publishes and execs a daemon that the first-writer-wins refusal turns away, and the
supervisor restarts it into the same refusal. That is the failure this record was written about, reached without any
bound having passed. The launcher has to act on the daemon it finds.

### Decision 5: the launcher replaces a client-started incumbent

1. **Lock scope.** A client that decides to spawn holds the marker lock from its locked marker read until its daemon
   answers on the socket or its ADR-049 spawn wait ends. The launcher holds the same lock from its first read of the
   existing marker until it execs. With both held that way only two orders exist: either the client's daemon already
   answers when the launcher probes, or the client finds the published marker and waits as Amendment 1 item 4 says. A
   client blocked on the lock is inside that wait, and its caller deadline returns the retryable "starting" kind.
2. **Its own job first.** Before publishing, the launcher reads the existing marker. If the marker carries its own label
   and names a live pid that answers on the socket, that daemon is its own job's, because the launcher's pid survives
   exec. A second launcher for a live job is a duplicate start: it refuses, leaves the marker byte-for-byte as it found
   it, and exits non-zero.
3. **Probe.** After publishing, still holding the lock, the launcher connects to the socket. If nothing accepts the
   connection it execs as today. If something accepts, the launcher sends it the daemon identity probe the client
   already uses. "A daemon answers" means the holder returned, within the probe timeout, a daemon response frame that
   decodes and reports its protocol version and served configuration. A mismatching configuration or protocol version
   still counts, since that is the daemon to replace; a socket that merely accepted does not. A holder that accepts but
   stays silent until the timeout, resets the connection, or returns a frame that does not decode is not a khive daemon
   as far as the launcher can prove: it is never signalled, and item 5 applies with the log line "incumbent is not a
   khive daemon". When a daemon answers, the launcher reads the holder's pid from the kernel's peer credentials of that
   connection (`SO_PEERCRED` on Linux, `LOCAL_PEERPID` on macOS). It never takes the pid from the PID file, which can
   name a dead process or a reused pid. Where the platform gives no peer pid, the launcher does not replace anything; it
   logs the reason and exits non-zero.
4. **Handover.** A holder that answered as a khive daemon and runs under the launcher's own uid is replaced. The
   launcher sends it SIGTERM, which runs the daemon's existing shutdown path, and waits up to one restart interval (the
   value it wrote into the marker) for the socket to stop answering and the pid to exit. Then it execs the supervised
   daemon. Clients connected to the old daemon see their connection close; their next request finds the marker and no
   socket, waits as Amendment 1 item 4 says, and connects to the supervised daemon.
5. **A holder that does not leave.** The launcher never sends SIGKILL. A holder still present after the wait, or one
   running under another uid (which the launcher cannot signal and does not try to), is logged with its pid, its uid and
   the time waited ("incumbent did not yield"). A holder that did not answer as a khive daemon is logged the same way
   with "incumbent is not a khive daemon" and is never signalled. The launcher leaves its marker in place and exits
   non-zero, so the supervisor retries after its interval. While that holder still answers, clients keep connecting to
   it.

Why this shape and not the other two:

- **The incumbent yields when it reads the marker.** Every running daemon would have to watch the marker. A daemon from
  a release that predates the marker never yields, and a rollout is exactly when such a daemon holds the socket. It
  also adds a poll loop to every daemon for a state only a launcher start creates.
- **The launcher adopts the incumbent.** The adopted daemon keeps the configuration of the client that started it and
  runs unsupervised, which are the two costs this record exists to remove. The arm's "the supervisor's pid equals the
  socket holder's pid" fails by construction.
- Socket activation stays the stronger form where the platform offers it (Alternatives considered, above), and it does
  not cover a deployment without it.

### Consequences of Amendment 2

- Amendment 1's sentence "The cost is that a client-started daemon can then hold the socket, and the supervisor's next
  start is refused by that incumbent, which is the original failure. It can only happen after the bound, and a log line
  names it." is replaced by: after the bound a client-started daemon can hold the socket until the supervisor's next
  start, which replaces it; the client logs the degradation when it starts the daemon, and the launcher logs the
  replacement.
- A replacement runs the old daemon's own shutdown path: its listening backlog closes, admitted requests get its bounded
  drain window to commit or roll back, and idle connections close. The gap a caller
  can see is at most one restart interval for the old daemon to leave plus the supervised daemon's bind time, and it is
  spent inside Amendment 1's wait.
- In a crash loop the socket alternates between owners: after the bound a client starts a daemon, the next supervised
  start replaces it and fails again, and clients wait out the bound again. Every alternation is logged on both sides. A
  crash loop is already an operator problem; this makes it a visible one rather than a silent one.
- The first-writer-wins refusal stays as it is. The launcher never binds past a live daemon; it removes the daemon
  first.

### Acceptance changes

The arm "The launcher writes before exec" stands and is decided by Decision 5 in the client-first order. Added:

- **Client first, then launcher.** Start from no socket, no PID file and no marker. Start a client with auto-spawn and
  wait until its daemon answers. Start the launcher. Within one restart interval plus the supervised bind time: exactly
  one daemon process exists; the socket holder's pid (read from peer credentials) equals the supervisor job's pid and
  the marker's pid; the client-started daemon left through its SIGTERM path; and the client's next request is served
  by the supervised daemon under the supervised configuration. Control: the same run with the handover disabled ends
  with the supervised start refused as "already serving this socket" and a socket holder whose pid differs from the
  job's. The control is what shows the arm tests the handover and not an ordering accident.
- **Started together, repeatedly.** Launcher and client start with a random offset between 0 and 1 second, twenty
  times. Every run ends with one daemon whose pid equals the job's.
- **Its own job is left alone.** With a supervised daemon serving, a second launcher for the same label refuses, the
  marker is byte-identical afterwards, and the serving daemon's pid is unchanged.
- **The PID file is not the pid source.** A PID file names a live unrelated process of the same uid and nothing answers
  on the socket. The launcher execs its daemon, and the unrelated process is still alive afterwards.
- **A silent holder is not signalled.** A same-uid stub accepts connections on the socket and never answers. The
  launcher does not send it SIGTERM, logs "incumbent is not a khive daemon" and exits non-zero; the marker stays and the
  stub is alive afterwards.
- **A holder that does not leave.** A holder that answers the identity probe but ignores SIGTERM (a test stub) makes the
  launcher wait one interval, log "incumbent did not yield", and exit non-zero; the marker stays, the holder still
  serves, and no second daemon exists.

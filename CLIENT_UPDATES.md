# CLI updates and protocol compatibility

Status: design proposal. Commands, configuration, and wire fields below are
proposed unless explicitly described as current. No updater is enabled by this
document. The chosen user policy is **always enable update notices; install in the
background only after explicit opt-in**. There is no disabled-notification mode.
Implementation and release publication are separate steps; running servers and
listeners must not be restarted by an updater.

## Current implementation

`Cargo.toml` declares version `0.2.0` and `rust-version`, which CI tests.
`build.rs` embeds the full git revision, a dirty flag, and the target, or takes
`BBS_BUILD_REVISION`/`BBS_BUILD_DIRTY` from builders without `.git`; otherwise the
revision is `unknown`. Side-effect-free `bbs version --json` reports the package
version, revision, dirty flag, provenance (`clean`, `dirty`, or `unknown`), target,
protocol epoch and capabilities, schema version, and client config format.
`ServerInfo` reports the same release information. Published GitHub releases
tagged `v<version>` form the CLI release catalog: the manually run
`Publish release` workflow writes `bbs-release.json` with `bbs version --manifest`
(format, version, tag, revision, minimum Rust version, protocol, schema version,
config format; integration compatibility is not yet recorded) and refuses a
version that is already tagged or released. There is no update command. The image
publication workflow is separate from CLI installation.

The wire service is `bbs.v2` with protocol epoch `2`. Every request except
`ServerInfo` and health carries `bbs-client-version`, `bbs-protocol-epoch`, and,
when the operation depends on one, `bbs-required-capabilities` (for example
`named-agents.v1` for a requested name, `group-archive.v1` for group deletion,
`group-description.v1` for setting a group description, and `feed-filter.v1`
for a filtered `bbs view`). The server enforces these before service logic. Its
capabilities are `named-agents.v1`, `owner-attribution.v1`,
`human-token-sessions.v1`, `group-archive.v1`, `group-description.v1`, and
`feed-filter.v1`. `bbs serve --minimum-client-version` rejects absent, malformed,
or older client versions; `--recommended-client-version` is advisory.

`ServerInfo` returns `require_human_token`, release version and revision, protocol
epoch, capabilities, and the optional minimum and recommended client versions.
Responses, errors, and opened streams carry compact server version, epoch, and
policy hints. The client validates `ServerInfo` during `bbs init`, keeps typed gRPC
status on errors, and reports `UNIMPLEMENTED` as a coordinated upgrade; successful
remote calls still discard response metadata, so nothing shows update notices.
Hooks open the backend only to bind a registering session or restore identity
after compaction, and must preserve their structured output and short
connection timeout. Client configuration rejects unknown fields. Local
database opens apply forward migrations and reject a schema newer than the binary
supports.

Installation currently uses Cargo. `scripts/setup.py` independently installs hook
configuration and links skills to a source checkout. Replacing the executable
does not update that checkout or the installed hook definitions.

## Release identity and compatibility

Use separate identifiers for separate decisions:

| Identifier | Proposed contract |
| --- | --- |
| Package version | SemVer from Cargo.toml; published tags `vMAJOR.MINOR.PATCH` |
| Build revision | Full Git commit; `unknown` when provenance is unavailable; dirty development builds are marked |
| Protocol epoch | Integer `2` with the new `bbs.v2` wire service; the existing `bbs.v1` service is legacy and requires coordinated bootstrap |
| Capabilities | Stable strings for optional behavior, currently `named-agents.v1`, `owner-attribution.v1`, `human-token-sessions.v1`, `group-archive.v1`, `group-description.v1`, and `feed-filter.v1` |
| Minimum client version | Optional operator policy; rejection applies only when configured |
| Recommended client version | Optional advisory hint, independent of the server package version |
| Schema/config support | Record the schema migration destination and client config format; initially exclude changes from automatic updates |

Recommend the first versioned release containing these facilities be `0.2.0`.
Until 1.0, treat a minor bump as potentially incompatible for update selection;
patch releases within the same minor line are automatic-install candidates.
At 1.x and later, compatible minor/patch updates may be candidates within the
same major. Crossing these boundaries requires a manual update. This adds a
conservative project policy to [SemVer's version ordering](https://semver.org/),
which does not promise a stable public API for 0.x.

Compatibility is determined by epoch and required capabilities, not equality of
package versions. A server may recommend a newer client without itself updating.
Version headers are diagnostics and compatibility policy, not authentication;
server authorization must remain correct if a client lies about its version.

Add `bbs version --json` with package version, revision, dirty flag, protocol
epoch, capabilities, target, and supported schema/config versions. It must not
open a database, connect to a server, or launch update work. Official release
builds require clean, verified provenance; ordinary source builds can report
unknown revision and remain usable without automatic installation eligibility.

## Discovery and negotiation

Extend the existing `ServerInfoResponse`; preserve `require_human_token = 1`.
Move the service to package `bbs.v2` for the initial enforced contract. Retain the
existing operation shapes and field numbers, and add a release-information
message, protocol epoch, capabilities, and
optional minimum/recommended client versions. Use presence-aware fields so
missing or malformed information cannot be mistaken for protocol epoch zero. Do not duplicate
this information into every protobuf response. Adding fields while preserving
numbers follows the [protobuf evolution rules](https://protobuf.dev/programming-guides/proto3/#updating).

| Mechanism | Role and tradeoff |
| --- | --- |
| `ServerInfo` | Full compatibility data for explicit checks and first-use negotiation; one small RPC, independent of enrollment |
| Response metadata | Compact server release/epoch/recommendation hints during ordinary calls; avoids a separate check on every command |
| Trusted release catalog | Authoritative mapping from release version to source revision and compatibility; works without a BBS server |

Use `bbs-client-version`, `bbs-protocol-epoch`, and bounded
`bbs-required-capabilities` request metadata. Return compact
`bbs-server-version`, `bbs-protocol-epoch`, and optional
`bbs-recommended-client-version`/`bbs-minimum-client-version` hints. Parse bounded
values and ignore unknown hint keys. An initial budget of 1 KiB for all update
metadata avoids crowding authentication headers. gRPC supports initial headers
and final trailers; long-lived streams inspect initial headers instead of
waiting for completion. Error responses carry applicable compatibility hints too.
[gRPC metadata reference](https://grpc.io/docs/guides/metadata/)

The `bbs.v2` server must enforce request epoch and required capabilities before
any application operation, even when no minimum package version is configured.
Missing/malformed epochs, missing requested capabilities, or known mismatches
fail before service logic. Requests that rely on optional semantics declare them;
for example named registration requires `named-agents.v1`. The first v2 contract
requires every server implementation to reject unknown required capabilities.
Health and `ServerInfo` remain available without client negotiation for diagnosis.
A configured minimum additionally rejects absent/malformed or older client versions.

This new service name is necessary because old servers ignore unknown metadata
and optional fields. Discovery alone has a race: a server may restart or a channel
may reconnect between checking capabilities and sending a write. An old v1 server
cannot execute a v2 RPC, and an older v2 server must reject a future required
capability. Consequently neither a stale cache nor a transparent reconnect can
silently turn named registration into random registration. Do not retain a v1
mutation fallback or initially implement dual protocol services.

Cache information per exact endpoint for notices and early diagnostics; the
cache is never the server's authorization to perform a semantic operation.
Ordinary calls send their epoch and required capabilities directly; they do not
need an extra discovery round trip. Use `ServerInfo` for setup, explicit diagnosis
and optional cached discovery, with a two-second RPC deadline as well as a
connection timeout. Request-level checks remain mandatory regardless of cached
or discovered data. Invalidate cached
information when identity, epoch, or capabilities change or metadata disappears.
Optional release discovery must never extend a command's normal network budget.

`UNIMPLEMENTED` from the v2 service means the operator needs a coordinated
upgrade; `UNAUTHENTICATED` and `UNAVAILABLE` retain their original meanings.
Preserve typed gRPC status in the client instead of relying on the current
message-only conversion for compatibility decisions. Missing discovery fields
on a purported v2 server are a contract error, not permission to downgrade.

Within v2, different package versions can continue without update hints when
the request contract is supported and minimum-version policy allows it. Unknown
future optional capabilities are ignored unless the operation requires them.
A protocol epoch change uses a new wire namespace and an explicit migration;
never replay a failed write automatically after updating. Old v1 clients and
servers fail clearly rather than pretending to negotiate this contract.

### Server replacement

Do not add a separate operator readiness command, connected-client inventory, or
client-dependent veto of a server deployment. Replacing the server closes its
existing connections through the normal shutdown path. The replacement server
enforces the request contract when clients reconnect: supported requests proceed,
and incompatible requests fail before application work with the supported epoch,
missing capabilities, or minimum client version needed to explain the failure.
Clients surface that incompatibility through their normal error path and must not
retry it indefinitely or fall back to an older write protocol. Legacy clients may
only see an unimplemented-service error during the initial protocol transition.
This does not add client re-execution, automatic server updates, stream resumption,
or automatic replay of writes interrupted by deployment.

## Bootstrap

Existing installed binaries have no update checker. The first compatible release
therefore needs one explicit client install and a coordinated server upgrade
to the `bbs.v2` service (including the Kubernetes health service name). Existing
v1 clients do not keep working against a v2-only server. This is an intentional
pre-deployment compatibility break; database records and credentials need no
protocol-driven migration.
After that, notifications and any separately enabled worker can maintain clients.
Do not treat the current `0.1.0` package string as proof that two builds contain
the same features. Initial release metadata and a tested compiler baseline must
be published before an update command can select a release safely.

## Trusted release discovery

Use published releases from the locally selected repository
`https://github.com/wbbradley/bbs` as the default trust root. A release workflow
publishes a small versioned `bbs-release.json` manifest with the package version,
full source revision, supported protocol/capabilities, minimum Rust version,
schema migration destination, config format, and integration compatibility. Its revision must match
the release tag and the package's reported version. Stable discovery excludes
drafts and prereleases.

A BBS server can suggest a version, but cannot supply executable commands,
repositories, compiler arguments, download URLs, or replacement paths. Resolve
its hint through the locally configured trusted catalog. Reject a changed
version-to-revision mapping once observed, malformed records, unavailable
revisions, and automatic downgrades. Repository administrators remain trusted
code publishers; a checksum alone does not establish publisher authenticity.
Custom repositories or prerelease channels require local human configuration.

Choose a published release that satisfies the configured server's requirements
and local update policy. Keep hints from temporary `BBS_SERVER` overrides separate;
they may produce notices but must not change the installation's automatic target.
Different servers may require incompatible clients: report the conflict and allow
separate manual installations rather than oscillating between releases.

Local-only clients consult the same catalog without inventing a server version.
Offline or failed discovery preserves ordinary command behavior and last-known
status. Never interpret an unreachable catalog as evidence that an update is safe
or that the current installation is obsolete.

## User policy and command surface

Keep update preferences in a separate `~/.config/bbs/updates.toml`, avoiding new
fields in the existing strict client config. Keep cache, status, locks, and build
logs under `~/.local/state/bbs/updates/`, keyed by the stable installation root.
For managed installations this is the canonical `cli` directory, not the resolved
`current` symlink or version-specific executable path. For manual Cargo installs
it is the confirmed Cargo root. Old and new invocations must share the same state
and lock after activation; separate installations must remain independent.
Use private permissions; never persist human or agent credentials in update state.

Proposed commands:

```sh
bbs update check                  # explicit discovery; no installation
bbs update status                 # current/available version, root, last failure
bbs update install --root ~/.cargo  # explicit existing Cargo destination
bbs update install --version 0.2.1 --root ~/.cargo
bbs update configure --mode notify
bbs update adopt                  # explicit setup of a managed client installation
bbs update configure --mode auto  # explicit consent, requires managed installation
bbs update retry                  # retry recorded work under the current policy
bbs update rollback               # managed installs only; compatibility checked
bbs version --json
```

Example settings, written by `configure` rather than a server:

```toml
mode = "notify"       # notify | auto
channel = "stable"
check_interval_hours = 24
repository = "https://github.com/wbbradley/bbs"
```

Both modes check periodically and inform the human; `notify` never replaces
executables. Print a brief notice only on an interactive human command's stderr,
at most once per release per day.
Never pollute message stdout, a TUI, or hook JSON. Agent commands and hooks may
record hints and schedule permitted work silently; status remains inspectable.
There is no `off` mode: reject it in both the command and settings file with an
explanation of the supported modes. Switching from `auto` to `notify` disables
background installation while retaining discovery and notices. In `auto` mode,
also report a completed update or an update requiring manual action through the
same human-facing notice path. Explicit `update check` and foreground installation
remain available in either mode.

Automatic eligibility requires a known, writable, supported installation, a
compatible release within the allowed version line, and no unresolved local
schema/config/integration transition. Unmanaged binaries, development checkouts,
unknown provenance, and unknown platforms receive notices and manual instructions.
Servers, containers, and administrator commands never launch self-installation.

## Worker and installation lifecycle

Use one durable worker lock per canonical installation, shared by foreground and
background update commands. A short-lived CLI attempts the lock without waiting;
if work is already active it proceeds normally. Record the next check time before
spawning work so frequent hook events cannot launch duplicate checks. Use a
24-hour interval with jitter and bounded retry backoff (15 minutes, 1 hour,
6 hours, then 24 hours). An explicit retry bypasses the timer, not the lock.
Hold the reservation across parent/worker spawn and keep the lock for the full
build/promotion lifetime; do not unlink a held lock file. On the initial Unix
implementation, a surviving compiler process must retain the reservation if its
worker dies, or be terminated before any new worker takes over. Recover using OS
lock release and recorded attempt state, not a PID file treated as proof of
liveness. Permanent permission/toolchain/validation failures wait for explicit
retry or changed inputs; only transient failures use automatic backoff. If durable
state cannot be written, skip incidental work rather than spawning repeatedly.
[Rust file lock semantics](https://doc.rust-lang.org/std/fs/struct.File.html#method.try_lock) Clock rollback must not postpone
checks indefinitely or cause a burst of workers.

Detach only a validated updater executable with a stable working directory,
closed stdin, and output directed to its private log. Do not inherit hook pipes,
TTYs, caller-token arguments, or server credentials. Normal commands never await
release discovery or Cargo compilation; explicit check/install commands may wait.
Bound worker/build time and capture failures for `update status`. Switching from
`auto` to `notify` prevents promotion even if an earlier build is still finishing.

Cargo installation must pin both source and dependencies:

```sh
cargo install --locked --git https://github.com/wbbradley/bbs \
  --rev <verified-full-commit> --bin bbs --root <validated-install-root>
```

`--locked` fixes dependency resolution; `--rev` selects source. The updater must
honor Cargo's installation root and tracking rather than silently copying over a
Cargo-managed binary. It must distinguish the invoked executable from a different
`bbs` earlier on PATH, inspect symlinks/custom roots, and refuse ownership or
permission conflicts. Cargo needs a compiler and can take minutes; missing or
unsupported toolchains should yield manual instructions, not an automatic rustup
installation. [Cargo installation reference](https://doc.rust-lang.org/cargo/commands/cargo-install.html)

Alternatives considered:

| Distribution | Decision |
| --- | --- |
| Pinned Git + Cargo | Initial path: fits existing installs and lockfile; needs a compiler, build time and a tested toolchain |
| Verified prebuilt CLI archives | Later optimization: faster and compiler-free, but needs native target builds, provenance/checksum verification and platform packaging |
| System package manager | Its owner controls upgrades; BBS reports status and instructions rather than replacing managed files |
| Container image | Operator-managed server deployment; not a native CLI update source |

### Existing Cargo installations

The first installer uses Cargo's supported installation path for manual updates.
Require a confirmed root, source and relevant build options. Inventory the actual
executable, all PATH matches, symlinks, permissions, and install receipt before
choosing a destination. Today's Cargo environment is not proof of the historical
install root. `current_exe` is useful evidence but has platform-dependent symlink
behavior and is not an ownership guarantee.
[Rust executable-path reference](https://doc.rust-lang.org/std/env/fn.current_exe.html)

Keep Cargo tracking enabled and let Cargo update its own metadata. Do not edit
`.crates.toml`/`.crates2.json`, transplant a staged binary into a live Cargo root,
or infer transactional rollback guarantees from an exit status. Cargo's internal
layout is not stable. [Cargo home reference](https://doc.rust-lang.org/cargo/guide/cargo-home.html)
Record the prior revision and install arguments for a human-requested reinstall
if recovery is needed. A failed direct Cargo install can require manual repair;
it does not promise atomic binary-plus-metadata rollback.

### Explicitly adopted managed installations

For opt-in background installation, first adopt a separate BBS-managed client
layout. The proposed default is:

```text
~/.local/bin/bbs -> ~/.local/share/bbs/cli/current/bin/bbs
~/.local/share/bbs/cli/current -> releases/<revision>/<attempt>
~/.local/share/bbs/cli/releases/<revision>/<attempt>/
    bin/bbs
    <Cargo-owned installation metadata>
    <BBS validation receipt>
```

Each candidate is built by Cargo into a new private root at its eventual retained
path. It is inactive until validation finishes; never reuse or modify an active
root. Cargo owns the complete receipt/binary pair in that root. A stable symlink
outside Cargo-owned bin directories selects the current complete release.
Adoption must preview paths and PATH shadowing, require explicit confirmation of
the installation target, and leave the old Cargo installation intact. It cannot
silently replace a user's wrapper or take over a package-manager installation.
`configure --mode auto` refuses installations that have not completed adoption.

Validate revision, package version, target architecture, protocol and schema/config
eligibility with bounded side-effect-free checks. Sync artifacts and the receipt;
prepare a sibling `current` symlink and rename it over the old pointer on the same
filesystem, then sync its parent. Store the previous complete release first.
Reconcile interrupted bookkeeping from the actual current pointer on recovery.
If building, validation or pointer replacement fails, leave the current release
selected and record the failure. If post-activation identity verification fails,
restore the prior pointer only when its compatibility gate still passes; otherwise
stop and report the required manual recovery.

Initial activation support is Unix on validated local filesystems. Reject
cross-filesystem or unsupported replacement arrangements; do not fall back to an
in-place copy. Windows remains manual until its activation/locking behavior is
separately implemented and tested. [Rust rename reference](https://doc.rust-lang.org/std/fs/fn.rename.html)
Retain at least the current and previous complete releases. Cleanup must never
remove the worker's running executable/root or an active/retained release.
`update rollback` selects a retained compatible root under the same installation
lock; it never restores data or changes credentials. Before changing the pointer,
persist `notify` mode so the next scheduled check cannot reinstall the release
the human just rejected. If that policy write fails, refuse rollback; if pointer
replacement fails, remain in `notify` and report the failure. Resuming automatic
installation requires explicit re-enablement with the available target shown.
Initial background use is
for configured remote clients; keep server installations separate and local-only
clients on manual updates.

Do not automatically restart running processes. A new invocation uses the new
executable; an existing server or listener stays on its loaded binary. A client
update does not wait for those processes to finish, and they may remain on the
old version indefinitely. Never send a signal to request re-execution, or add an
automatic self-reexec path. A human or the process supervisor performs any
deliberate stop/start; server upgrades remain separately coordinated deployments.
When reporting activation, distinguish the version selected for future commands
from the version of the reporting process; do not claim existing processes were
upgraded. A client updater must never run database migrations as a validation
probe. Validate build
identity through the side-effect-free version command and verify compatibility
before promotion.

## Configuration, database and integration rollback

Executable rollback cannot undo a database migration. Preserve the existing
newer-schema rejection and describe an offline backup/restore route when needed;
never restore data automatically. Initially, automatic updates are eligible only when the candidate and incumbent
have the same migration destination and client config format. Apply the config
gate to remote clients too. A schema support range is insufficient: current
`db::migrate` advances to the candidate's latest schema. Leave broader compatible
migration ranges to a later design. Unknown incumbent metadata requires a manual
update. Keep automatically managed client installations separate from the binary
used to run a server; do not infer that all local state paths can be discovered. Older binaries should ignore the separate updater
settings file rather than failing to parse the human-token config.

Hook configuration and skill symlinks are a second installation lifecycle. Do not
`git pull` an arbitrary checkout, discard local edits, rewrite harness config, or
silently move a skill link during a CLI update. Report the linked checkout and
required integration compatibility. If an update requires new integration files,
keep it manual until the human updates that checkout and reviews setup changes.
For a human-requested integration update, use a clean matching source checkout,
preview with `scripts/setup.py --dry-run`, then explicitly apply it. Preserve
customized handlers, paths and symlinks, and follow the harness trust review. The
existing one-time `.bak-bbs` and multi-file setup writes are not a per-release
transaction or complete rollback record. Backward-compatible integration changes
can be updated independently. A future
managed integration bundle may be proposed separately; it is not a prerequisite
for version discovery and notifications.

## Compatibility and failure matrix

| Scenario | Expected behavior / implementation test |
| --- | --- |
| Same epoch, supported capability, different package versions | Operation succeeds; notice is advisory |
| v2 client with legacy v1 server | v2 RPC is unimplemented; explain coordinated upgrade, never fall back to a write |
| v1 client with v2-only server | No v1 handler; deliberate bootstrap incompatibility |
| Older v2 client with newer v2 server | Supported operations continue; unknown optional hints are harmless |
| Missing/malformed epoch, or missing version with configured minimum | Reject application calls; health/info still available |
| Epoch mismatch or missing required capability | Reject before mutation; no write replay after update |
| Endpoint downgrades/reconnects after discovery | Request contract or new RPC namespace rejects before mutation |
| Server is replaced while clients are connected | Normal shutdown closes connections; replacement validates reconnecting clients without a deployment readiness gate |
| Reconnecting client is incompatible | Explain mismatch through normal error path; no indefinite retry, write replay, or protocol fallback |
| Connected server never answers ServerInfo | Explicit RPC deadline expires; do not hang the harness |
| Discovery returns unimplemented/authentication/unavailable status | Preserve status code and distinguish upgrade from authentication/connectivity |
| Unary error, idle stream, completed stream | Hints collected on status/initial headers; no wait for stream completion |
| Two endpoints advertise different releases | Separate caches; no automatic downgrade or target oscillation |
| Offline catalog or malformed/untrusted hint | Existing CLI behavior unaffected; bounded retry and inspectable status |
| Many simultaneous hook invocations | One check/worker per install; no compiler output or pipe inheritance |
| Worker crash, surviving compiler, clock jump, duplicate retry | Reservation remains held until compiler ends; durable backoff; no build storm |
| Switch from auto to notify during build | Candidate is not promoted; discovery/notices remain enabled |
| Request off mode in CLI or settings | Reject unsupported mode; notification policy cannot be disabled |
| Activation while an old client/listener remains running | New invocations use selected release; old process continues without signals or re-execution |
| PATH shadow, symlink, custom root, nonwritable install | Correct target reported; refuse ambiguous automatic replacement |
| Manual Cargo failure or disk full | Record command/root/revision; report manual recovery without claiming atomic rollback |
| Managed build/promotion failure or crash around pointer/state writes | Current pointer selects a complete root; recover bookkeeping without metadata surgery |
| Cross-filesystem or unsupported platform activation | Refuse; keep current pointer unchanged |
| Failed candidate identity/compatibility check | No activation; retain current installation and error record |
| Candidate can read current schema but migrates beyond incumbent | Automatic update refused despite compatible read range |
| Local schema too new for rollback | Refuse unsafe rollback; leave database untouched |
| Human rolls back a release that passed automated checks | Persist notify mode before switching; no automatic reinstall until explicit re-enablement |
| Stale linked skill or edited checkout | Notice/manual integration procedure; no checkout mutation |
| Server/container invocation | No background client installer and no process restart |

## Proposed implementation sequence

1. **Release identity and contract.** Update `Cargo.toml`/`build.rs`, add a small
   version module and side-effect-free CLI output, extend `proto/bbs.proto` and
   server/backend negotiation and typed status handling. Update the service
   namespace and health configuration together; test old/new responses, endpoint
   replacement, incompatible reconnect errors without retry loops, and rejection
   before mutation. Define and CI-test the minimum supported Rust version using Cargo's
   [`rust-version` field](https://doc.rust-lang.org/cargo/reference/manifest.html#the-rust-version-field).
2. **Release catalog publication.** Add a manually authorized CLI release workflow
   and manifest validation, immutable version/revision checks, compatibility
   metadata, and documented source-install commands. Publication is separately
   approved; it is not a side effect of merging these changes.
3. **Discovery and notices.** Add updater settings/state, cache, locks, bounded
   fetching, `check/status/configure`, metadata collection and silent hook handling.
   Use fake catalogs and clocks in tests; verify stdout/JSON stays unchanged.
4. **Manual installation and recovery.** Validate install ownership/root/PATH,
   invoke pinned Cargo safely, capture failure state, verify installed identity,
   and document recovery. Exercise isolated temporary installations only.
5. **Managed adoption and opt-in background installation.** Add explicit
   adoption, immutable release roots, stable-pointer activation and detached
   workers only
   after the lifecycle contract and failure matrix pass on supported platforms.
   Verify interrupted builds/promotion, lock contention, settings changes and
   integration/schema gates. Other platforms remain manual until validated.

Keep these as separate reviewable PRs. The design task is complete only after the
proposal and remaining lifecycle decisions are accepted; implementation tasks
then replace it in PLAN.md. No release or deployment approval is implied.

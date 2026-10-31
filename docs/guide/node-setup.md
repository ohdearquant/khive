# Node Setup

Create device keys in the login Keychain, import an enrolment result, and confirm
contact keys without editing key material by hand. Configuration changes preserve
unrelated content and take effect after a daemon restart.

## What a node is

A node is this Mac acting as a named device. Its device keys live in the login
Keychain of the user who runs the commands and never leave it. The configuration
table `[channel.node]` stays off until the node is assigned. The `kkernel node`
commands prepare that table, create the keys, import the enrolment result and
record the contacts you trust. The table itself is documented in
[Configuration](../configuration.md#channelnode--node-channel).

## Requirements

- macOS. On any other platform the commands refuse with exit code 4 and
  `node setup requires macOS`.
- A `kkernel` build with the `macos-keychain` feature. Without it the commands
  refuse with exit code 4 and
  `node setup requires a build with the macos-keychain feature`.
- One explicit configuration file, selected with `--config PATH` or the
  `KHIVE_CONFIG` environment variable. The file is never discovered. With no
  selection the commands refuse with exit code 2 and
  `select the configuration file with --config`.

## The four steps

The commands are `init`, `assign`, `pin` and `status`.

### 1. Initialize this Mac's node keys

```bash
kkernel node init --config ~/.khive/khive.toml \
  --service-url https://hub.example.com \
  --realm <realm> --actor <label> \
  --out node-bundle.json
```

| Flag            | Meaning                                                            |
| --------------- | ------------------------------------------------------------------ |
| `--service-url` | Origin of the hub. Required unless `--recover`.                    |
| `--realm`       | Realm of this node. Required unless `--recover`.                   |
| `--actor`       | Label of this node's actor. Required unless `--recover`.           |
| `--slug`        | Node slug. Default `node`.                                         |
| `--namespace`   | Namespace. Default `local`.                                        |
| `--recover`     | Re-read the public bundle of an incomplete initialization.         |
| `--out`         | File for the public bundle. Default `-`, which is standard output. |

`init` writes a disabled `[channel.node]` table, adds one key item to the login
Keychain and records its fingerprint. It only adds: it never replaces or deletes
a key item. If an item already exists for this node the command refuses with
exit code 4; after an interrupted run use `--recover`. `--recover` accepts only
`--out` besides the global `--config`.

The public bundle goes to the file named by `--out`, or to standard output.
A file is created only if it does not already exist; an existing path is refused
before anything changes. The fingerprint is shown on standard error. When the
bundle goes to a file, standard output carries a one-line JSON receipt with
`state` `initialized`, `enabled` `false`, the configuration path, the bundle file
and the fingerprint.

### 2. Send the public bundle to the hub operator

Send the bundle file (or the printed JSON object) to the hub operator, and
compare the fingerprint with them on another channel. The bundle holds public
keys only. The operator returns the enrolment result for your node.

### 3. Import the enrolment result

```bash
kkernel node assign --config ~/.khive/khive.toml --input enrolment.json
```

`assign` checks that the result matches the keys held in the Keychain: the
realm, the key epoch and the fingerprint. It then fills the node binding in the
configuration file, enables the table, and stores public share-card metadata
beside the configuration file. The input file may be at most 64 KiB. The result
reports that a daemon restart is required.

### 4. Confirm each contact

```bash
kkernel node pin --config ~/.khive/khive.toml --card contact-card.json
```

Run `pin` once for every contact you want to trust, with the share card that
contact gave you. The node must be assigned first. `pin` looks the contact up on
the hub with one signed request and shows, on standard error, the contact
address, label, realm, device, key epoch and the full fingerprint. Compare that
fingerprint with the contact on another channel.

The confirmation is read from the controlling terminal, never from standard
input, so a card can be piped in. Only the exact answer `yes` followed by Enter
confirms; anything else, an over-long line or end of input cancels. With no
terminal the command refuses with exit code 3.

After you confirm, `pin` looks the contact up again. If the contact changed in
between, nothing is written and it exits with code 2. A contact that is already
pinned with the same keys changes nothing and is reported as unchanged.

## Status and exports

```bash
kkernel node status --config ~/.khive/khive.toml
kkernel node status --config ~/.khive/khive.toml --bundle --out node-bundle.json
kkernel node status --config ~/.khive/khive.toml --card --out node-card.json
```

Without options `status` prints one line of JSON describing local state:
`state` (one of `absent`, `prepared`, `initialized`, `assigned`, `disabled`),
the configuration path, `enabled`, the service URL, realm, actor, namespace,
slug, node address, device, key epoch, fingerprint, the number of pins and the
configuration digest of an enabled table. `keychain` is `matches` when the key
item was opened and its public keys agree with the recorded fingerprint, and
`not_checked` otherwise. `daemon` and `delivery` are always `not_checked`.

- `--bundle` exports the public bundle again, for example after an interrupted
  `init`. It needs a key item recorded in the configuration.
- `--card` exports this node's public share card, to give to a contact. It needs
  an assigned node and the saved enrolment metadata.
- `--bundle` and `--card` exclude each other. `--out FILE` is accepted only with
  one of them, and the file is created only if it does not already exist; a
  receipt with `state` `exported` is printed.

## What changes in the configuration file

Every command that edits the file replaces only the `[channel.node]` table and
its pins. Comments and every unrelated setting are kept as written, and the file
is replaced atomically, so a failed save leaves the previous file in place. If
the file changes while a command is running, or another setup command holds it,
the command refuses with exit code 6 and you run it again.

Changes take effect after a daemon restart. A running daemon keeps the table it
started with.

## Exit codes

A refusal prints one line, `node: <message>`, to standard error.

| Code | Meaning                                                                                     |
| ---- | ------------------------------------------------------------------------------------------- |
| 0    | The command succeeded.                                                                      |
| 2    | Usage or state refusal: bad option, no `--config`, wrong state, or the contact changed.     |
| 3    | The pin confirmation was declined or cancelled, or no terminal is available.                |
| 4    | Platform, build or key problem: no macOS, no `macos-keychain` feature, or Keychain failure. |
| 5    | The hub lookup during `pin` was refused or failed.                                          |
| 6    | Local file problem: a file could not be read or written, or the configuration changed.      |

## What the commands never do

- They never put key material in the configuration file or in any output. The
  bundle and the share card hold public keys only.
- `init` makes no network request.
- `status` checks nothing remote and reports `daemon` and `delivery` as
  `not_checked`.
- They never delete or replace a key item, and never overwrite an output file.

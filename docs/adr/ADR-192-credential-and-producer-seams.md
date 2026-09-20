# ADR-192: Credential Seam and Producer Seams for Network Packs

- **Status**: Accepted (design)
- **Governing rule**: this repository holds seams; credential production is product and lives outside it
- **Date**: 2026-09-20
- **Depends on**: [ADR-191](ADR-191-web-pack-ontology-and-operations.md) (web pack; D3 host-set
  credential binding, D4 receipts, D6 extension seam), [ADR-028](ADR-028-pack-scoped-backends.md)
  (configuration as the placement surface)
- **Relates to**: [ADR-175](ADR-175-web-pack.md) Amendment 1 (the credential binding this record
  generalises)

## Context

ADR-191 D3 binds a credential to a host set inside the web pack's own configuration, with the value
read from one environment variable and presented as one request header. Three things that an
operator or a downstream product needs are not expressible in that shape: a credential kind other than
a static header (basic authentication, a cookie set, a signing key held by a request hook), a provider
other than the process environment (a file today; an operating-system keychain or a vault tomorrow),
and a way for code outside this repository to participate in a request (sign it, refresh a token,
attach a session) without re-implementing HTTP and losing receipts and blobs.

The governing rule for this repository is that packs hold ontology, relation rules and operations, and
seams for consumers. Credential production (login flows, token refresh, key custody, cookie
production) is product behaviour and stays outside. This record defines the seams and nothing behind
them. A second constraint shapes the cut: this repository is permissively licensed, so anything an
operator needs to run the software for other people (credential custody, multi-tenant configuration,
deployment-grade providers) is out of it by decision, not by omission. The seam is the product
boundary.

## Decision

### S1. Credential seam: named credentials, four kinds, providers by trait

Credentials are declared once, at the runtime level, and referenced by name everywhere else.

```toml
[[credentials]]
name = "partner-token"
kind = "header"            # header | basic | cookie_jar | signing_key
provider = "env"           # the only provider shipped here; others through the trait
env_var = "PARTNER_TOKEN"
header = "X-Api-Key"          # header kind only

[[web.credentials]]        # binding, not a value: which named credential a host set receives
credential = "partner-token"
hosts = ["api.example.com"]
```

| kind          | what the provider yields                    | how a network pack applies it                                    |
| ------------- | ------------------------------------------- | ---------------------------------------------------------------- |
| `header`      | one header value                            | sets the named request header                                    |
| `basic`       | `user:password`                             | `Authorization: Basic` per RFC 7617                              |
| `cookie_jar`  | an opaque cookie set scoped to the host set | sends matching cookies; never inspects, persists or writes them  |
| `signing_key` | opaque key material                         | never read by the pack; handed to a request hook (S2) that signs |

Providers implement one trait: resolve a named credential to its material at request time, report
absence as an error naming the credential (never the value), and declare whether the material may be
cached in memory for the process lifetime. The trait also carries an optional `update(name, material)`
that only the `cookie_jar` kind honours: the pack never calls it, so a jar resolved from a static
provider stays static; a request hook (S2) that reads a response's `Set-Cookie` may call it, which is
how a producer outside this repository keeps a session live through the seam. The only provider shipped in this repository is `env`,
which is what local development and the test suite need. A file, keychain, vault, or token-refreshing
provider implements the same trait outside this repository and is selected by `provider = "<name>"`
through the host-binary composition of ADR-191 D6.

Host-set semantics are unchanged from ADR-191 D3: hostname entries match by suffix, IP literals match
exactly, and a credential is presented only to hosts in its set after redirect resolution.

**Redaction rule** (extends ADR-191 D4). A receipt records the credential name, its kind, and the names
of the request headers sent. It never records a header value, a cookie value, a signature, or key
material. A diagnostic log carries the credential name and kind only; no prefix of the material appears
in any log or receipt. The first-six-characters-plus-length form is an operator display for an explicit
redacted read, never a log line. The guarantee is structural: no code path hands credential material to
the store or the log. The store's secret gate, which refuses a note, property or tag whose value is
shaped like credential material, is a backstop behind that guarantee, not the guarantee. A receipt that
would need to carry a value to be complete is incomplete by design.

### S2. Request hook on fetch

A network pack exposes one hook point per request: a decorator registered at host-binary composition
that may add or replace request headers before the request is sent and may read response headers after
it returns. The hook runs inside the pack's egress discipline (after address classification and
allow-list checks, before the body ceilings) and inside the receipt discipline (whatever it adds is
recorded by header name under S1's redaction rule). The hook receives the resolved credential material
for `signing_key` bindings and is the only place such material is readable. Hooks are registered by
name and bound to host sets the same way credentials are; an unbound hook runs for no request. A hook
may add or replace request headers but may not change the URL, the method or the host. Headers a hook
adds pass the same request-header allow-list as the pack's own, and the response headers it reads are
the ones the response-header allow-list admits. Egress classification is not re-run after the hook,
because the hook cannot move the request. A hook may call the provider's `update` for a `cookie_jar`
credential it is bound to; that is the only write path into a jar. This
repository ships the trait and the binding, no hook implementation, and no helper that composes one.

### S3. Row minting as a library surface

The web pack's identity and persistence logic is public library code, not private to its verbs:
canonicalise a URL (ADR-191 D1), derive the deterministic identities of `site`, `page` and
`resource`, mint or re-type those rows, store a body as a blob, and write a receipt note. A second
producer of web rows (a rendered fetch, a browser-driven session, an offline importer) calls these
functions and yields rows indistinguishable from `web.fetch`'s, with the same identities, the same
edges, and the same receipt shape. The surface is versioned with the crate; it carries no network code
and no credential material.

### S4. Pack factories at composition

ADR-191 D6 already adds the library entry through which a host binary registers additional pack
factories. S1's providers and S2's hooks are registered through the same entry, so a downstream
composition is one function call: base packs, extra packs, credential providers, request hooks.

## Out of scope, by decision

Login automation, cookie production, OAuth clients and token refresh, request signing schemes, key
custody, keychain and vault adapters, and any browser or renderer pack. Each is a consumer of S1–S4 and
lives outside this repository. A browser pack, if one is ever needed, has no ontology of its own: it is
a second producer of web rows through S3 and would be its own record when a consumer names a row it
needs.

## Acceptance

In every arm the resolved credential value is a high-entropy string of at least 32 characters, so
asserting its absence by substring is meaningful. C1 and C4 test the structural guarantee; C8 tests the
backstop.

- C1 a `header` credential from `env` is presented to a host in its set and to no host outside it;
  the receipt carries the credential name and header name and not the value (assert absence by
  substring over the whole receipt and log).
- C2 a provider that cannot resolve a name fails the request with the credential name in the error
  and no partial request sent.
- C3 a `basic` credential produces a correct `Authorization` header; the receipt carries the header
  name only.
- C4 a `cookie_jar` credential is sent to matching hosts and never appears in any store row (search by
  substring over notes and properties returns nothing).
- C5 a `signing_key` credential is unreadable by the pack (no accessor) and reaches a registered hook;
  with no hook bound the request proceeds unsigned and the receipt says so.
- C6 a hook bound to host set A does not run for host B; a hook that adds a header is recorded by name.
- C6b a hook-driven `update` on a `cookie_jar` credential is visible on the next request to the same
  host set; the pack's own code path never calls `update` (assert by a provider that records callers).
- C7 rows minted through S3 by a test producer equal rows minted by `web.fetch` for the same URL and
  body: same ids, same edges, same receipt fields.
- C8 the secret gate refuses a note carrying a value that a provider resolved, using the value from
  C1 as the probe.

## Consequences

Network packs stop owning secrets: they own bindings by name. Every consumer that needs more than a
static token builds it outside this repository against a seam that is tested here. Receipts stay
complete and safe to persist, because completeness is defined as names, not values.

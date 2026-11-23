use khive_types::{HandlerDef, IdResolutionMode, ParamDef, VerbCategory, Visibility};

const fn p(
    name: &'static str,
    ty: &'static str,
    required: bool,
    description: &'static str,
) -> ParamDef {
    ParamDef {
        name,
        param_type: ty,
        required,
        description,
        resolution_mode: IdResolutionMode::NotApplicable,
    }
}
const REPO: ParamDef = p(
    "repo",
    "string",
    true,
    "Absolute allowlisted repository with configured remote expectations.",
);
const HEAD: ParamDef = p(
    "expected_head",
    "string",
    true,
    "Exact 40-hex platform head SHA. Omission and null refuse.",
);
const NUMBER: ParamDef = p("number", "integer", true, "Positive pull request number.");
const BODY: ParamDef = p("body", "string", true, "Body text; may be empty.");
const SESSION: ParamDef = crate::local_vocab::SESSION;

pub(crate) const PUSH: HandlerDef = HandlerDef {
    name: "git.push",
    description: "Push an exact local SHA with a server-side expected-remote compare after fast-forward proof. Expected remote null means must not exist. Optional force must be boolean: true always refuses force_denied before transport; false and omission preserve the ordinary push. Caller remote/refspec overrides refuse. HTTPS uses actor-only credentials. An absolute path or file:/// remote configured with slug=\"\" uses no credential or actor row and records credential.source=none. Durable receipt, no retries. Requires a git that advertises the `git reflog write ` subcommand, which first shipped in Git 2.51: the receipt marker is written with it, and without it the verb refuses unsupported_toolchain before any credential or network use, recording the version found and the missing capability. The check reads the capability, never the version number.",
    visibility: Visibility::Verb,
    category: VerbCategory::Commissive,
    params: &[
        REPO,
        p("branch", "string", true, "Branch to push."),
        p("expected_local", "string", true, "Required exact 40-hex local branch head."),
        p("expected_remote", "string|null", true, "Required exact remote SHA, or explicit null only when the remote branch must not exist. Omission is invalid_params."),
        p("force", "boolean", false, "Validation only: true always refuses force_denied before transport. False or omission requests an ordinary push; null and non-booleans are invalid_params."),
        SESSION,
    ],
};

pub(crate) const FETCH: HandlerDef = HandlerDef {
    name: "git.fetch",
    description: "Fetch one branch or one pull request's head from the configured remote. A branch name writes refs/remotes/origin/<ref>; pull/<N> always means pull request N's head and writes refs/fetched/pull/<N>. The transfer runs by URL in a scratch repository whose configuration the pack writes, so the clone's configuration, hooks and remotes are never read; received objects are checked, and the clone's own tips are offered so only missing objects travel. Received objects land in a directory of their own and enter the clone's object store only after the transfer completed and the ref names the expected commit: a refused, failed or timed-out transfer and a mismatch leave the store as it was. The destination moves only by a compare-and-swap from the value read before the fetch: ref_moved (another writer changed it) and ref_locked (another writer holds its lock) leave it unchanged and are safe to retry. Optional expected_remote is the exact 40-hex SHA the fetched ref must have; any other value refuses expected_remote_mismatch with the destination unchanged. remote_ref_absent means the remote has no such ref; objects_refused means the remote sent an object the check rejects. HTTPS uses the dispatching actor credential as git.push does; an absolute path or file:/// remote configured with slug=\"\" uses none. The transfer is bounded at 300 seconds. Durable receipt. Returns ref, sha, previous_sha, remote and receipt_id.",
    visibility: Visibility::Verb,
    category: VerbCategory::Commissive,
    params: &[
        REPO,
        p("ref", "string", true, "A branch name, or pull/<N> for pull request N's head."),
        p("expected_remote", "string", false, "Exact 40-hex SHA the fetched ref must have. Omission accepts whatever the remote holds; null is invalid_params."),
        SESSION,
    ],
};

pub(crate) const PR_OPEN: HandlerDef = HandlerDef {
    name: "git.pr_open",
    description: "Open a pull request after checking configured slug, visibility and exact platform head. Local mappings refuse remote_scheme before resolving credentials; PR verbs require HTTPS. Uses the dispatching actor credential: [git_write] actors.<actor>.credential_ref is resolved through credential_resolver (by default the login keychain item with that service name), never stored, and an unmapped actor or a failed resolver refuses actor_unmapped before any network call. The resolved token must belong to the actor configured platform_identity or the act refuses platform_identity_mismatch. Returns number, url, head_sha and receipt_id.",
    visibility: Visibility::Verb,
    category: VerbCategory::Commissive,
    params: &[
        REPO,
        p("head", "string", true, "Head branch."),
        p("base", "string", true, "Base branch."),
        p("title", "string", true, "Pull request title."),
        BODY,
        HEAD,
        SESSION,
    ],
};

pub(crate) const PR_REVIEW: HandlerDef = HandlerDef {
    name: "git.pr_review",
    description: "Submit a review bound to expected_head. Local mappings refuse remote_scheme before any credential or platform read; PR verbs require HTTPS. Approval requires a different opening actor, credential reference and platform account; fork approvals additionally require git.pr_review.fork. Returns review_id, head_sha, state and receipt_id.",
    visibility: Visibility::Verb,
    category: VerbCategory::Commissive,
    params: &[
        REPO,
        NUMBER,
        p("verdict", "string", true, "approve, request_changes or comment."),
        BODY,
        HEAD,
        SESSION,
    ],
};

pub(crate) const PR_MERGE: HandlerDef = HandlerDef {
    name: "git.pr_merge",
    description: "Merge with a platform head comparison after caller policy and current-head approval by another account. Local mappings refuse remote_scheme before any credential or review read; PR verbs require HTTPS. The merge presents as the dispatching actor credential, resolved the same way as git.pr_open. The approval precondition is an approving review at expected_head from a platform account other than the LAST PUSHER of that head, not other than the author. A repository row may list merge_refusals: opener refuses a merge dispatched by the account or actor that opened the pull request (merge_by_opener) and last_pusher refuses one dispatched by the login on the newest push receipt for expected_head (merge_by_last_pusher), both before any platform write and named on the receipt; with no list a merge dispatched by the opener is not refused. Forks require git.pr_merge.fork; no administrator bypass is requested. Returns merged_head_sha, merged_sha and receipt_id.",
    visibility: Visibility::Verb,
    category: VerbCategory::Commissive,
    params: &[
        REPO,
        NUMBER,
        p("method", "string", true, "squash or merge."),
        p("subject", "string", true, "Merge commit subject."),
        BODY,
        HEAD,
        SESSION,
    ],
};

pub(crate) const PR_STATE: HandlerDef = HandlerDef {
    name: "git.pr_state",
    description: "Read what the platform says about one pull request: state, merged, draft, opener, head and base, the platform's own mergeable and mergeable_state, the check runs and commit statuses at the head, and each reviewer's latest decisive review with whether it is at the head. Reads with the dispatching actor's credential after the allowlist match and the policy decision; local mappings refuse remote_scheme before any credential read. Writes no receipt and returns no verdict: a required-check ruling is the caller's read of the rows. Check runs are the platform's latest per name, at most 500, with total and truncated. A combined status with total 0 says pending and means no status was posted. Names are returned with control and format characters removed and bounded in length; text_altered says when that changed one.",
    visibility: Visibility::Verb,
    category: VerbCategory::Assertive,
    params: &[REPO, NUMBER],
};

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
    description: "Push an exact local SHA with a server-side expected-remote compare after fast-forward proof. Expected remote null means must not exist. All caller force/remote/refspec overrides refuse. Actor-only credentials, durable receipt, no retries.",
    visibility: Visibility::Verb,
    category: VerbCategory::Commissive,
    params: &[
        REPO,
        p("branch", "string", true, "Branch to push."),
        p("expected_local", "string", true, "Required exact 40-hex local branch head."),
        p("expected_remote", "string|null", true, "Required exact remote SHA, or explicit null only when the remote branch must not exist. Omission is invalid_params."),
        SESSION,
    ],
};

pub(crate) const PR_OPEN: HandlerDef = HandlerDef {
    name: "git.pr_open",
    description: "Open a pull request after checking configured slug, visibility and exact platform head. Uses the dispatching actor credential: [git_write] actors.<actor>.credential_ref is resolved through credential_resolver (by default the login keychain item with that service name), never stored, and an unmapped actor or a failed resolver refuses actor_unmapped before any network call. The resolved token must belong to the actor configured platform_identity or the act refuses platform_identity_mismatch. Returns number, url, head_sha and receipt_id.",
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
    description: "Submit a review bound to expected_head. Approval requires a different opening actor, credential reference and platform account; fork approvals additionally require git.pr_review.fork. Returns review_id, head_sha, state and receipt_id.",
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
    description: "Merge with a platform head comparison after caller policy and current-head approval by another account. The merge presents as the dispatching actor credential, resolved the same way as git.pr_open. The approval precondition is an approving review at expected_head from a platform account other than the LAST PUSHER of that head, not other than the author. A repository row may list merge_refusals: opener refuses a merge dispatched by the account or actor that opened the pull request (merge_by_opener) and last_pusher refuses one dispatched by the login on the newest push receipt for expected_head (merge_by_last_pusher), both before any platform write and named on the receipt; with no list a merge dispatched by the opener is not refused. Forks require git.pr_merge.fork; no administrator bypass is requested. Returns merged_head_sha, merged_sha and receipt_id.",
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

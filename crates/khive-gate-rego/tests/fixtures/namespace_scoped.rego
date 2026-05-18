package khive.gate

import rego.v1

default decision := {"decision": "deny", "reason": "default deny"}

decision := {
    "decision":    "allow",
    "obligations": [{"kind": "audit", "tag": sprintf("verb.%s", [input.verb])}],
} if {
    input.actor.kind == "user"
    input.namespace  == "local"
}

decision := {"decision": "deny", "reason": "anonymous callers cannot write"} if {
    input.actor.kind == "anonymous"
    input.verb       == "create"
}

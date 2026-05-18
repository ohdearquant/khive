package khive.gate

import rego.v1

default decision := {"decision": "deny", "reason": "default — only `search` is allowed"}

decision := {"decision": "allow", "obligations": []} if {
    input.verb == "search"
}

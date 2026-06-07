# comm — Inter-agent Messaging

Structured messaging between agents, lambdas, and namespaces. Messages are stored as `message` notes
with direction tracking, read status, and threaded conversations.

## Skills

| Skill    | Description                                     |
| -------- | ----------------------------------------------- |
| `send`   | Send a message to a recipient                   |
| `inbox`  | Check inbound messages and mark them read       |
| `reply`  | Reply to a message with automatic threading     |
| `thread` | View a full conversation thread chronologically |

## Quick start

```
# Send a message
request(ops="comm.send(to=\"local\", content=\"deployment complete\", subject=\"status update\")")

# Check inbox
request(ops="comm.inbox()")

# Reply to a message
request(ops="comm.reply(id=\"<message-id>\", content=\"acknowledged\")")

# View thread
request(ops="comm.thread(id=\"<thread-root-id>\")")
```

## How it works

Every `send` creates two copies: an **outbound** copy in the sender's namespace and an **inbound**
copy in the recipient's namespace. `inbox` returns only inbound messages. `read` marks an inbound
message as read. Replies are threaded by a canonical `thread_id` shared across both copies.

Cross-namespace messaging is denied until ADR-018 ACL policy is implemented. Currently, sender and
recipient must be in the same namespace.

## Requirements

Requires the `kg` pack (notes infrastructure) and the `comm` pack.

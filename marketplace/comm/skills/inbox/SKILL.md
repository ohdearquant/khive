---
description: Check inbound messages and mark them read — filter by unread, read, or all status.
---

# Inbox

View inbound messages in the caller's namespace. Messages arrive via `comm.send` or `comm.reply` and
start as unread. Use `comm.read` to mark them read after processing.

## Workflow

### 1. Check unread messages

```
request(ops="comm.inbox()")
```

Returns only unread inbound messages (default `status="unread"`):

```json
{
  "messages": [
    {
      "id": "a1b2c3d4",
      "full_id": "a1b2c3d4-...",
      "kind": "message",
      "content": "deployment complete",
      "properties": { "from": "local", "to": "local", "direction": "inbound", "read": false, ... }
    }
  ],
  "count": 1
}
```

### 2. Filter by status

```
request(ops="comm.inbox(status=\"all\")")
request(ops="comm.inbox(status=\"read\")")
request(ops="comm.inbox(status=\"unread\")")
```

### 3. Limit results

```
request(ops="comm.inbox(limit=5)")
```

Default limit is 20, max 200.

### 4. Mark a message as read

After processing a message, mark it read using its `full_id`:

```
request(ops="comm.read(id=\"<message-full-id>\")")
```

Only inbound messages can be marked read — `comm.read` on an outbound message is rejected.

### 5. Process inbox in batch

Check inbox and mark all as read in sequence:

```
request(ops="comm.inbox(limit=10)")
```

Then for each message:

```
request(ops="comm.read(id=\"<full_id>\")")
```

## Patterns

### Session start inbox check

```
request(ops="comm.inbox(limit=10)")
```

Read before doing work — messages may contain blockers, status updates, or task assignments.

### Triage by subject

Inbox messages include `properties.subject`. Scan subjects to prioritize which messages to read
first.

## Anti-patterns

- **Ignoring the inbox.** Messages from other agents/lambdas may contain blockers or coordination
  signals.
- **Marking outbound messages as read.** Only inbound messages have read semantics — outbound read
  is rejected.
- **Polling without limit.** In active namespaces, always set a limit to avoid large payloads.

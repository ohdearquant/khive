# khive-pack-operation

Operation graphs for khive. An operation type is published once, as data: its slots (who or
what takes part, one identity or many), its steps (which slot acts, after which steps, whether
every identity of a repeating slot must act or one is enough) and the rules that ask or skip a
step on recorded facts and on other steps. Each step is published as its own step type, and the
operation type pins each one by digest.

An operation is opened in the caller's namespace against one pinned type version, never in the
namespace that holds the type. It grows only by appended records (bindings, facts and acts),
each with the time it was observed and the time it was recorded. Records are never changed or
removed, and a caller key makes a retry land once.

`operation.frontier` reads, as of a time, every act owed now and who supplies it, every act
blocked and what it waits on, and every required slot nobody has bound yet, grouped by
supplier. A step every rule skips is bypassed, and the steps after it wait on what it waited on.

| Verb | Writes |
| --- | --- |
| `operation.define` | operation types and their step types |
| `operation.open` | an operation and the bindings known when it opens |
| `operation.bind` | one binding |
| `operation.record` | bindings, facts and acts |
| `operation.frontier` | nothing |

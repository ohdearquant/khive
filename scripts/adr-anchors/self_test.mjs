import { resolve } from "node:path";

export function runSelfTests(check) {
  const root = resolve("anchor-fixture");
  const a = resolve(root, "docs/adr/ADR-001-fixture.md");
  const b = resolve(root, "docs/adr/ADR-002-target.md");
  const headings = `# ADR-002: Target
## **Heading** with \`code\`!
## Repeat
## Repeat
## Repeat-1
## Repeat
Setext heading
--------------
## 中文标题
<a id="explicit-anchor"></a>
<!-- <a id="commented-anchor"></a> -->
\`\`\`markdown
## Fake fenced heading
[not a link](#deliberately-absent)
\`\`\`
    ## Indented code heading
`;
  const positive = `# ADR-001: Fixture
## Same file
[same](#same-file)
[cross](ADR-002-target.md#heading-with-code)
[duplicate](ADR-002-target.md#repeat-1)
[collision](ADR-002-target.md#repeat-1-1)
[later duplicate](ADR-002-target.md#repeat-2)
[setext](ADR-002-target.md#setext-heading)
[unicode](ADR-002-target.md#%E4%B8%AD%E6%96%87%E6%A0%87%E9%A2%98)
[explicit](ADR-002-target.md#explicit-anchor)
[reference][dest]

[dest]: ADR-002-target.md#repeat

[external](https://example.invalid/ADR-003-remote.md#missing)
[other kind](../guide.md#not-an-adr-fragment)
[code](#same-file "a title")
\`[inline example](#not-a-real-link)\`
<!-- [comment](#not-a-real-link) -->
~~~~
[code example](#not-a-real-link)
\`\`\`
~~~~
`;
  const clean = check(new Map([[a, positive], [b, headings]]));
  if (clean.errors.length || clean.checked !== 10) {
    throw new Error(
      `anchor self-test positive arm failed: ${JSON.stringify(clean)}`,
    );
  }
  for (
    const [name, source, needle] of [
      ["same-file", "[bad](#missing-same)", "missing-same"],
      [
        "reference-style",
        "[bad][missing-ref]\n\n[missing-ref]: ADR-002-target.md#missing-reference",
        "missing-reference",
      ],
      [
        "cross-file",
        "[bad](ADR-002-target.md#renamed-heading)",
        "renamed-heading",
      ],
      ["duplicate suffix", "[bad](ADR-002-target.md#repeat-99)", "repeat-99"],
      [
        "fenced fake heading",
        "[bad](ADR-002-target.md#fake-fenced-heading)",
        "fake-fenced-heading",
      ],
      [
        "indented fake heading",
        "[bad](ADR-002-target.md#indented-code-heading)",
        "indented-code-heading",
      ],
      [
        "comment fake anchor",
        "[bad](ADR-002-target.md#commented-anchor)",
        "commented-anchor",
      ],
      ["missing file", "[bad](ADR-999-missing.md#heading)", "absent"],
      ["case-sensitive fragment", "[bad](ADR-002-target.md#Repeat)", "#Repeat"],
    ]
  ) {
    const result = check(
      new Map([[a, positive + "\n" + source], [b, headings]]),
    );
    if (result.errors.length !== 1 || !result.errors[0].includes(needle)) {
      throw new Error(
        `anchor self-test ${name} must-fail arm failed: ${
          JSON.stringify(result)
        }`,
      );
    }
  }
}

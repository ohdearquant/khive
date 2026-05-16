/**
 * Research session orchestration.
 *
 * A session is a stateful exploration of a topic — recursive paper reading,
 * entity extraction, gap detection. Calls into khive-mcp for KG operations
 * and into LLM SDKs (Anthropic/OpenAI) for agent reasoning.
 *
 * Placeholder for v0.1 — actual orchestration logic lands in v0.2.
 */

import type { KhiveMcpClient } from "../mcp/client.ts";

export interface SessionConfig {
  topic: string;
  maxDepth: number;
  namespace: string;
}

export class ResearchSession {
  constructor(
    private readonly mcp: KhiveMcpClient,
    private readonly config: SessionConfig,
  ) {}

  run(): Promise<void> {
    // TODO(v0.2): recursive exploration, agent team, extraction pipeline
    return Promise.reject(new Error("Not yet implemented — landing in v0.2"));
  }
}

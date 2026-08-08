import type { ReviewBundle } from "@/lib/review-bundle";

export type ReviewSourceCapabilities = Readonly<{
  gitReads: boolean;
  khiveReads: boolean;
  githubWrites: boolean;
  wasm: boolean;
}>;

/**
 * Server-facing boundary for review data. Implementations may read files or
 * invoke trusted processes; client components receive only the returned bundle.
 */
export interface ReviewSource {
  readonly id: string;
  readonly capabilities: ReviewSourceCapabilities;
  load(): Promise<ReviewBundle>;
}

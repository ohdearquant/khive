/** RFC4122 v4-like UUID via crypto.randomUUID. */
export function randomUuid(): string {
  return crypto.randomUUID();
}

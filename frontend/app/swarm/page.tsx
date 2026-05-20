import { SwarmProvider } from "./_components/SwarmContext";
import SwarmOverview from "./_components/SwarmOverview";

// ---------------------------------------------------------------------------
// SwarmOverviewPage — server component wrapper
// Per ADR-045 §D9, this is a React Server Component that hands off to the
// SwarmProvider (client component tree) for polling + interactive state.
// ---------------------------------------------------------------------------

interface SwarmPageProps {
  searchParams?: Promise<{ namespace?: string }>;
}

export default async function SwarmPage({ searchParams }: SwarmPageProps) {
  const params = await (searchParams ?? Promise.resolve({ namespace: undefined }));
  const namespace = params.namespace ?? "local";

  return (
    <SwarmProvider initialNamespace={namespace}>
      <SwarmOverview />
    </SwarmProvider>
  );
}

export const metadata = {
  title: "Swarm — khive",
  description: "Real-time agent swarm telemetry dashboard",
};

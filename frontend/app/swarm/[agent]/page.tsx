import Link from "next/link";
import { AgentDrilldownProvider } from "../_components/AgentDrilldownContext";
import AgentDrilldownView from "../_components/AgentDrilldownView";

// ---------------------------------------------------------------------------
// AgentDrilldownPage — server component wrapper
// Per ADR-045 §D9 the per-agent drilldown lives at /swarm/:agent.
// ---------------------------------------------------------------------------

interface AgentDrilldownPageProps {
  params: Promise<{ agent: string }>;
  searchParams?: Promise<{ namespace?: string }>;
}

export default async function AgentDrilldownPage({
  params,
  searchParams,
}: AgentDrilldownPageProps) {
  const { agent } = await params;
  const resolvedSearch = await (searchParams ?? Promise.resolve({ namespace: undefined }));
  const namespace = resolvedSearch.namespace ?? "local";
  const agentName = decodeURIComponent(agent);

  return (
    <div>
      <nav className="border-b border-neutral-200 bg-white px-4 py-3 text-sm text-neutral-500">
        <Link href="/swarm" className="hover:text-blue-600">
          ← Swarm
        </Link>
        <span className="mx-2">/</span>
        <span className="font-mono font-semibold text-neutral-900">{agentName}</span>
      </nav>

      <AgentDrilldownProvider agentName={agentName} namespace={namespace}>
        <AgentDrilldownView agentName={agentName} />
      </AgentDrilldownProvider>
    </div>
  );
}

export async function generateMetadata({ params }: AgentDrilldownPageProps) {
  const { agent } = await params;
  const agentName = decodeURIComponent(agent);
  return {
    title: `${agentName} — Swarm — khive`,
  };
}

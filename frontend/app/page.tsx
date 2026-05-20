import Link from "next/link";

export default function Dashboard() {
  return (
    <div className="p-8">
      <h1 className="mb-1 text-2xl font-bold text-white">Dashboard</h1>
      <p className="mb-8 text-sm text-neutral-500">
        khive research knowledge graph runtime
      </p>

      <div className="grid grid-cols-2 gap-4 xl:grid-cols-4">
        <Link
          href="/kg"
          className="flex flex-col gap-2 rounded border border-neutral-800 bg-neutral-900 p-5 transition-colors hover:border-neutral-700 hover:bg-neutral-800"
        >
          <div className="text-2xl">◎</div>
          <div className="font-medium text-neutral-100">KG Explorer</div>
          <div className="text-xs text-neutral-500">
            Browse, search, and visualize the entity graph
          </div>
        </Link>

        <Link
          href="/tasks"
          className="flex flex-col gap-2 rounded border border-neutral-800 bg-neutral-900 p-5 transition-colors hover:border-neutral-700 hover:bg-neutral-800"
        >
          <div className="text-2xl">▤</div>
          <div className="font-medium text-neutral-100">GTD Board</div>
          <div className="text-xs text-neutral-500">
            Six-column kanban for the task queue
          </div>
        </Link>

        <Link
          href="/swarm"
          className="flex flex-col gap-2 rounded border border-neutral-800 bg-neutral-900 p-5 transition-colors hover:border-neutral-700 hover:bg-neutral-800"
        >
          <div className="text-2xl">◉</div>
          <div className="font-medium text-neutral-100">Swarm</div>
          <div className="text-xs text-neutral-500">
            Agent telemetry and run monitoring
          </div>
        </Link>
      </div>
    </div>
  );
}

import {
  configuredShowcaseAnalysis,
  loadMaterializedShowcaseAnalysis,
  type MaterializedShowcaseAnalysis,
  resolveShowcaseAnalysisRegistry,
  ShowcaseAnalysisError,
  showcaseAnalysisErrorBody,
  type ShowcaseAnalysisRegistry,
} from "@/lib/server/materialized-showcase-source";

type RouteContext = Readonly<{
  params: Promise<Readonly<{ id: string }>>;
}>;

type RegistryLoader = () =>
  | ShowcaseAnalysisRegistry
  | Promise<ShowcaseAnalysisRegistry>;
type AnalysisLoader = (
  id: string,
  registry: ShowcaseAnalysisRegistry,
) => Promise<Pick<MaterializedShowcaseAnalysis, "bytes" | "etag">>;

const responseHeaders = {
  "cache-control": "private, no-store",
  "x-content-type-options": "nosniff",
} as const;

export function createShowcaseAnalysisGet(
  loadRegistry: RegistryLoader = resolveShowcaseAnalysisRegistry,
  loadAnalysis: AnalysisLoader = loadMaterializedShowcaseAnalysis,
) {
  return async function get(
    _request: Request,
    context: RouteContext,
  ): Promise<Response> {
    const { id } = await context.params;
    try {
      const registry = await loadRegistry();
      if (!configuredShowcaseAnalysis(id, registry)) {
        throw new ShowcaseAnalysisError("NOT_CONFIGURED");
      }
      const analysis = await loadAnalysis(id, registry);
      return new Response(analysis.bytes.buffer, {
        status: 200,
        headers: {
          ...responseHeaders,
          "content-type": "application/json; charset=utf-8",
          etag: analysis.etag,
          "x-khive-analysis-id": id,
          "x-khive-analysis-source": "khive-db-snapshot",
        },
      });
    } catch (error) {
      const safeError = error instanceof ShowcaseAnalysisError
        ? error
        : new ShowcaseAnalysisError("ANALYSIS_UNAVAILABLE");
      return Response.json(showcaseAnalysisErrorBody(safeError), {
        status: safeError.status,
        headers: responseHeaders,
      });
    }
  };
}

export function createShowcaseAnalysisCatalogGet(
  loadRegistry: RegistryLoader = resolveShowcaseAnalysisRegistry,
) {
  return async function get(): Promise<Response> {
    try {
      const registry = await loadRegistry();
      const entries = registry.entries
        .map(({ analysis_id, canonical_url }) => ({
          analysis_id,
          canonical_url,
        }))
        .sort((left, right) =>
          left.analysis_id < right.analysis_id
            ? -1
            : left.analysis_id > right.analysis_id
            ? 1
            : 0
        );
      return new Response(
        JSON.stringify({
          schema_version: "khive.showcase.catalog.v1",
          entries,
        }),
        {
          status: 200,
          headers: {
            ...responseHeaders,
            "content-type": "application/json; charset=utf-8",
          },
        },
      );
    } catch (error) {
      const safeError = error instanceof ShowcaseAnalysisError
        ? error
        : new ShowcaseAnalysisError("ANALYSIS_UNAVAILABLE");
      return Response.json(showcaseAnalysisErrorBody(safeError), {
        status: safeError.status,
        headers: responseHeaders,
      });
    }
  };
}

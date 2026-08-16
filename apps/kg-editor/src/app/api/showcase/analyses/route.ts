import {
  createShowcaseAnalysisCatalogGet,
} from "@/lib/server/showcase-analysis-route";

export const runtime = "nodejs";
export const dynamic = "force-dynamic";

export const GET = createShowcaseAnalysisCatalogGet();

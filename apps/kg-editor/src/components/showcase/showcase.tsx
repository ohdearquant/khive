"use client";

import { ArrowRight, GitBranch, Search, ShieldCheck } from "@/icons";
import Link from "next/link";
import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type FormEvent,
} from "react";

import { RepoShowcase } from "@/components/showcase/repo-showcase";
import { DataState } from "@/components/data-state";
import {
  loadPreferredShowcaseBundle,
  ShowcaseAnalysisNotFoundError,
  type LoadedShowcaseBundle,
  type ShowcaseBundleSource,
} from "@/lib/adapters/preferred-showcase-source";
import {
  loadShowcaseAnalysisCatalog,
  mergeShowcaseRegistry,
  type ShowcaseAnalysisCatalogResult,
} from "@/lib/adapters/showcase-analysis-catalog";
import type { RepoBundle } from "@/lib/repo-bundle";
import {
  resolveShowcaseRepository,
  type ShowcaseRegistryEntry,
} from "@/lib/showcase-registry";
import { parseRepositoryLocation } from "@/lib/repository-location";

type LoadState =
  | Readonly<{ status: "loading"; entry: ShowcaseRegistryEntry }>
  | Readonly<{ status: "ready"; entry: ShowcaseRegistryEntry; loaded: LoadedShowcaseBundle }>
  | Readonly<{
      status: "miss";
      normalizedUrl: string;
      cause: "registry" | "analysis-not-found";
    }>
  | Readonly<{ status: "invalid"; reason: string }>
  | Readonly<{ status: "error"; reason: string }>;

type CatalogState = Readonly<{
  status: "loading" | ShowcaseAnalysisCatalogResult["status"];
  registry: readonly ShowcaseRegistryEntry[];
  message: string;
}>;

const bundleCache = new Map<string, Promise<LoadedShowcaseBundle>>();
const STATIC_SHOWCASE_REGISTRY = mergeShowcaseRegistry([]);
const staticShowcaseEntry = STATIC_SHOWCASE_REGISTRY[0];
if (!staticShowcaseEntry) {
  throw new Error("The repository showcase requires one curated static entry.");
}
const DEFAULT_SHOWCASE_ENTRY = staticShowcaseEntry;

function loadEntry(entry: ShowcaseRegistryEntry): Promise<LoadedShowcaseBundle> {
  const cacheKey = [
    entry.id,
    entry.analysisId ?? "static",
    entry.assetPath ?? "no-asset",
  ].join("|");
  const existing = bundleCache.get(cacheKey);
  if (existing) return existing;
  const pending = loadPreferredShowcaseBundle(entry).catch((error: unknown) => {
    bundleCache.delete(cacheKey);
    throw error;
  });
  bundleCache.set(cacheKey, pending);
  return pending;
}

function sourceLabel(source: ShowcaseBundleSource): string {
  return source === "khive-db-snapshot" ? "khive DB snapshot" : "curated static fallback";
}

function replaceRepositoryQuery(
  repository?: string,
  clearInvestigation = true,
) {
  const query = new URL(window.location.href);
  if (repository) query.searchParams.set("repo", repository);
  else query.searchParams.delete("repo");
  if (clearInvestigation) {
    query.searchParams.delete("at");
    query.searchParams.delete("module");
    query.searchParams.delete("view");
  }
  const search = query.searchParams.size ? `?${query.searchParams.toString()}` : "";
  window.history.replaceState(null, "", `${query.pathname}${search}${query.hash}`);
}

export function Showcase() {
  const [input, setInput] = useState(DEFAULT_SHOWCASE_ENTRY.canonicalUrl);
  const [state, setState] = useState<LoadState>({
    status: "loading",
    entry: DEFAULT_SHOWCASE_ENTRY,
  });
  const [catalog, setCatalog] = useState<CatalogState>({
    status: "loading",
    registry: STATIC_SHOWCASE_REGISTRY,
    message: "Discovering configured repository analyses.",
  });
  const [labels, setLabels] = useState<RepoBundle["capability"]["labels"] | null>(null);
  const loadSequence = useRef(0);

  const beginLoad = useCallback((
    entry: ShowcaseRegistryEntry,
    clearInvestigation = true,
  ) => {
    const sequence = ++loadSequence.current;
    setInput(entry.canonicalUrl);
    setState({ status: "loading", entry });
    replaceRepositoryQuery(entry.canonicalUrl, clearInvestigation);
    void loadEntry(entry)
      .then((loaded) => {
        if (loadSequence.current !== sequence) return;
        setLabels(loaded.bundle.capability.labels);
        setState({ status: "ready", entry, loaded });
      })
      .catch((error: unknown) => {
        if (loadSequence.current !== sequence) return;
        if (error instanceof ShowcaseAnalysisNotFoundError) {
          setState({
            status: "miss",
            normalizedUrl: error.canonicalUrl,
            cause: "analysis-not-found",
          });
          return;
        }
        setState({
          status: "error",
          reason: error instanceof Error
            ? error.message
            : "The showcase bundle could not be loaded.",
        });
      });
  }, []);

  useEffect(() => {
    let cancelled = false;
    const sequence = ++loadSequence.current;
    void loadShowcaseAnalysisCatalog().then((catalogResult) => {
      if (cancelled || loadSequence.current !== sequence) return;
      const registry = mergeShowcaseRegistry(catalogResult.entries);
      setCatalog({
        status: catalogResult.status,
        registry,
        message: catalogResult.message,
      });

      const parsed = parseRepositoryLocation(new URL(window.location.href));
      const repositoryIssue = parsed.issues.find((issue) =>
        issue.parameter === "repo"
      );
      if (repositoryIssue) {
        setState({ status: "invalid", reason: repositoryIssue.message });
        return;
      }

      const requestedRepository = parsed.location.repository ??
        DEFAULT_SHOWCASE_ENTRY.canonicalUrl;
      const lookup = resolveShowcaseRepository(requestedRepository, registry);
      setInput(requestedRepository);
      if (lookup.status === "invalid") {
        setState(lookup);
        return;
      }
      if (lookup.status === "miss") {
        setState({ ...lookup, cause: "registry" });
        return;
      }

      beginLoad(lookup.entry, false);
    });
    return () => {
      cancelled = true;
      loadSequence.current += 1;
    };
  }, [beginLoad]);

  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (catalog.status === "loading") return;
    const lookup = resolveShowcaseRepository(input, catalog.registry);

    if (lookup.status === "invalid") {
      loadSequence.current += 1;
      replaceRepositoryQuery();
      setState(lookup);
      return;
    }
    if (lookup.status === "miss") {
      loadSequence.current += 1;
      replaceRepositoryQuery();
      setState({ ...lookup, cause: "registry" });
      return;
    }

    beginLoad(lookup.entry);
  }

  function openCuratedExample() {
    const curated = catalog.registry.find((entry) => entry.assetPath) ??
      DEFAULT_SHOWCASE_ENTRY;
    beginLoad(curated);
  }

  const busy = catalog.status === "loading" || state.status === "loading";
  const selectedEntryId = state.status === "loading" || state.status === "ready"
    ? state.entry.id
    : "";
  const analysisSource = state.status === "ready"
    ? sourceLabel(state.loaded.source)
    : state.status === "loading"
    ? "Pending"
    : "Not loaded";
  const analysisStatus = catalog.status === "loading"
    ? "Discovering configured repository analyses."
    : state.status === "loading"
    ? `Opening ${state.entry.canonicalUrl}.`
    : state.status === "ready"
    ? `Loaded ${state.entry.canonicalUrl} from ${sourceLabel(state.loaded.source)}.`
    : state.status === "miss"
    ? `No repository analysis is available for ${state.normalizedUrl}.`
    : state.status === "invalid"
    ? `Repository URL is invalid: ${state.reason}`
    : `Repository analysis failed: ${state.reason}`;

  return (
    <div className="repo-shell">
      <header className="repo-topbar">
        <Link className="repo-brand" href="/" aria-label="khive repository showcase home">
          <span className="repo-brand-mark" aria-hidden="true"><i /><i /><i /></span>
          <strong>khive</strong>
          <span>{labels?.product}</span>
        </Link>
        <nav className="repo-product-nav" aria-label="Product navigation">
          <Link className="active" href="/"><Search aria-hidden="true" /> {labels?.product ?? "Showcase"}</Link>
          <Link href="/review"><ShieldCheck aria-hidden="true" /> KG review</Link>
        </nav>
        <span className="repo-readonly"><GitBranch aria-hidden="true" /> {state.status === "ready" ? sourceLabel(state.loaded.source) : "…"}</span>
      </header>

      <main>
        <section className="repo-hero" aria-labelledby="repo-hero-title">
          <div className="repo-hero-copy">
            <span className="repo-eyebrow">History meets structure</span>
            <h1 id="repo-hero-title">See how a repository <em>really</em> moves.</h1>
            <p>
              Explore modules, dependencies, change hotspots, hidden coupling, and ownership from
              one precomputed, reproducible graph bundle.
            </p>
          </div>
          <form className="repo-url-form" onSubmit={submit} noValidate aria-busy={busy}>
            <label className="repo-analysis-picker" htmlFor="repository-analysis">
              <span>Repository analysis</span>
              <select
                id="repository-analysis"
                value={selectedEntryId}
                disabled={catalog.status === "loading"}
                aria-busy={busy}
                onChange={(event) => {
                  const entry = catalog.registry.find((candidate) =>
                    candidate.id === event.target.value
                  );
                  if (entry) beginLoad(entry);
                }}
              >
                {!selectedEntryId && <option value="">Select a repository analysis</option>}
                {catalog.registry.map((entry) => (
                  <option key={entry.id} value={entry.id}>
                    {entry.canonicalUrl}
                  </option>
                ))}
              </select>
            </label>
            <label htmlFor="repository-url">Public repository URL</label>
            <div>
              <Search aria-hidden="true" />
              <input
                id="repository-url"
                inputMode="url"
                spellCheck={false}
                value={input}
                onChange={(event) => setInput(event.target.value)}
                placeholder={labels?.input_placeholder ?? "https://github.com/owner/repository"}
              />
              <button type="submit" disabled={catalog.status === "loading"}>
                {labels?.lookup_action ?? "Open analysis"} <ArrowRight aria-hidden="true" />
              </button>
            </div>
            <section className="repo-analysis-statuses" aria-label="Repository analysis state">
              <span>Source <output aria-label="Analysis source">{analysisSource}</output></span>
              <p role="status" aria-label="Repository catalog status">{catalog.message}</p>
              <p role="status" aria-label="Repository analysis status">{analysisStatus}</p>
            </section>
            <small>A configured server snapshot is built from khive history and code-map databases. The browser never clones, ingests, or opens SQLite.</small>
          </form>
        </section>

        <div className="repo-result" aria-busy={state.status === "loading"}>
          {state.status === "loading" && (
            <DataState
              className="repo-state-card"
              state="loading"
              title="Opening the repository analysis"
              message="A validated materialized snapshot belongs here; no repository process is running in this request."
            />
          )}
          {state.status === "invalid" && (
            <DataState
              className="repo-state-card"
              state="error"
              title="Repository lookup could not start"
              message={state.reason}
            />
          )}
          {state.status === "miss" && (
            <DataState
              className="repo-state-card"
              state="empty"
              title={state.cause === "analysis-not-found"
                ? "Configured repository analysis is unavailable"
                : labels?.miss_title ?? "No curated showcase bundle matches this repository"}
              message={state.cause === "analysis-not-found"
                ? `The configured snapshot is no longer available and has no approved static fallback. · ${state.normalizedUrl}`
                : `${labels?.miss_body ?? "Curated repository showcase bundles belong here."} · ${state.normalizedUrl}`}
              action={{ label: "Use the curated khive example", onClick: openCuratedExample }}
            />
          )}
          {state.status === "error" && (
            <DataState
              className="repo-state-card"
              state="error"
              title="Repository analysis could not be opened"
              message={state.reason}
            />
          )}
          {state.status === "ready" && (
            <RepoShowcase
              key={state.entry.id}
              bundle={state.loaded.bundle}
              analysisSource={state.loaded.source}
            />
          )}
        </div>
      </main>
    </div>
  );
}

"use client";

import { ArrowRight, GitBranch, Search, ShieldCheck } from "@/icons";
import Link from "next/link";
import { useEffect, useRef, useState, type FormEvent } from "react";

import { RepoShowcase } from "@/components/showcase/repo-showcase";
import { DataState } from "@/components/data-state";
import {
  loadPreferredShowcaseBundle,
  readOperatorShowcaseAccessToken,
  type LoadedShowcaseBundle,
  type ShowcaseBundleSource,
} from "@/lib/adapters/preferred-showcase-source";
import type { RepoBundle } from "@/lib/repo-bundle";
import {
  resolveShowcaseRepository,
  SHOWCASE_REGISTRY,
  type ShowcaseRegistryEntry,
} from "@/lib/showcase-registry";

type LoadState =
  | Readonly<{ status: "loading"; entry: ShowcaseRegistryEntry }>
  | Readonly<{ status: "ready"; entry: ShowcaseRegistryEntry; loaded: LoadedShowcaseBundle }>
  | Readonly<{ status: "miss"; normalizedUrl: string }>
  | Readonly<{ status: "invalid"; reason: string }>
  | Readonly<{ status: "error"; reason: string }>;

const bundleCache = new Map<string, Promise<LoadedShowcaseBundle>>();

function loadEntry(entry: ShowcaseRegistryEntry): Promise<LoadedShowcaseBundle> {
  // The cache must never outlive the authorization that filled it: the key
  // carries the current session token, so removal or rotation misses the
  // cache and the protected route re-authorizes, instead of a previously
  // authorized private snapshot being served from module memory. The raw
  // token adds no exposure here: sessionStorage already holds it and both
  // are readable by the same origin's scripts.
  const accessToken = readOperatorShowcaseAccessToken();
  const cacheKey = `${entry.id}\u0000${accessToken ?? ""}`;
  const existing = bundleCache.get(cacheKey);
  if (existing) return existing;
  const pending = loadPreferredShowcaseBundle(entry, fetch, {
    accessToken,
  }).catch((error: unknown) => {
    bundleCache.delete(cacheKey);
    throw error;
  });
  bundleCache.set(cacheKey, pending);
  return pending;
}

function sourceLabel(source: ShowcaseBundleSource): string {
  return source === "khive-db-snapshot" ? "khive DB snapshot" : "curated static fallback";
}

function replaceRepositoryQuery(repository?: string) {
  const query = new URL(window.location.href);
  if (repository) query.searchParams.set("repo", repository);
  else query.searchParams.delete("repo");
  const search = query.searchParams.size ? `?${query.searchParams.toString()}` : "";
  window.history.replaceState(null, "", `${query.pathname}${search}${query.hash}`);
}

export function Showcase() {
  const defaultEntry = SHOWCASE_REGISTRY[0];
  const [input, setInput] = useState(defaultEntry.canonicalUrl);
  const [state, setState] = useState<LoadState>({ status: "loading", entry: defaultEntry });
  const [labels, setLabels] = useState<RepoBundle["capability"]["labels"] | null>(null);
  const loadSequence = useRef(0);

  useEffect(() => {
    const sequence = ++loadSequence.current;
    void loadEntry(defaultEntry)
      .then((loaded) => {
        setLabels(loaded.bundle.capability.labels);
        if (loadSequence.current === sequence) {
          setState({ status: "ready", entry: defaultEntry, loaded });
        }
      })
      .catch((error: unknown) => {
        if (loadSequence.current === sequence) {
          setState({
            status: "error",
            reason: error instanceof Error ? error.message : "The showcase bundle could not be loaded.",
          });
        }
      });
  }, [defaultEntry]);

  function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const lookup = resolveShowcaseRepository(input);
    const sequence = ++loadSequence.current;

    if (lookup.status === "invalid") {
      replaceRepositoryQuery();
      setState(lookup);
      return;
    }
    if (lookup.status === "miss") {
      replaceRepositoryQuery();
      setState(lookup);
      return;
    }

    setState({ status: "loading", entry: lookup.entry });
    replaceRepositoryQuery(lookup.normalizedUrl);
    void loadEntry(lookup.entry)
      .then((loaded) => {
        if (loadSequence.current === sequence) {
          setLabels(loaded.bundle.capability.labels);
          setState({ status: "ready", entry: lookup.entry, loaded });
        }
      })
      .catch((error: unknown) => {
        if (loadSequence.current === sequence) {
          setState({
            status: "error",
            reason: error instanceof Error ? error.message : "The showcase bundle could not be loaded.",
          });
        }
      });
  }

  function openCuratedExample() {
    const sequence = ++loadSequence.current;
    setInput(defaultEntry.canonicalUrl);
    setState({ status: "loading", entry: defaultEntry });
    replaceRepositoryQuery(defaultEntry.canonicalUrl);
    void loadEntry(defaultEntry)
      .then((loaded) => {
        if (loadSequence.current === sequence) {
          setLabels(loaded.bundle.capability.labels);
          setState({ status: "ready", entry: defaultEntry, loaded });
        }
      })
      .catch((error: unknown) => {
        if (loadSequence.current === sequence) {
          setState({
            status: "error",
            reason: error instanceof Error ? error.message : "The showcase bundle could not be loaded.",
          });
        }
      });
  }

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
          <form className="repo-url-form" onSubmit={submit} noValidate>
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
              <button type="submit">{labels?.lookup_action ?? "…"} <ArrowRight aria-hidden="true" /></button>
            </div>
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
              title={labels?.miss_title ?? "No curated showcase bundle matches this repository"}
              message={`${labels?.miss_body ?? "Curated repository showcase bundles belong here."} · ${state.normalizedUrl}`}
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
          {state.status === "ready" && <RepoShowcase bundle={state.loaded.bundle} analysisSource={state.loaded.source} />}
        </div>
      </main>
    </div>
  );
}

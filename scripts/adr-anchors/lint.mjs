// CommonMark parsing and GitHub heading IDs; no hand-written Markdown lexer.
import MarkdownIt from "markdown-it";
import GithubSlugger from "github-slugger";
import { Parser } from "htmlparser2";
import { basename, dirname, join, relative, resolve, sep } from "node:path";
import { runSelfTests } from "./self_test.mjs";

const markdown = new MarkdownIt({ html: true });

export function documentReferences(source) {
  const anchors = new Set();
  const links = [];
  const slugger = new GithubSlugger();
  let heading = null;
  const parser = new Parser({
    onopentag(name, attrs) {
      if (attrs.id !== undefined) anchors.add(attrs.id);
      if (name === "a") {
        if (attrs.name !== undefined) anchors.add(attrs.name);
        if (attrs.href !== undefined) links.push(attrs.href);
      }
      if (/^h[1-6]$/.test(name)) heading = { name, text: "" };
      if (name === "img" && heading) heading.text += attrs.alt ?? "";
    },
    ontext(text) {
      if (heading) heading.text += text;
    },
    onclosetag(name) {
      if (heading?.name === name) {
        anchors.add(slugger.slug(heading.text));
        heading = null;
      }
    },
  }, { decodeEntities: true });
  parser.write(markdown.render(source));
  parser.end();
  return { anchors, links };
}

// A pure document map lets the same production checker exercise synthetic
// broken links on every invocation without writing files or spawning children.
export function checkDocuments(documents) {
  const parsed = new Map();
  const errors = [];
  let checked = 0;
  const read = (path) => {
    if (!parsed.has(path) && documents.has(path)) {
      parsed.set(path, documentReferences(documents.get(path)));
    }
    return parsed.get(path);
  };
  for (const path of [...documents.keys()].sort()) {
    for (const href of read(path).links) {
      if (
        !href.includes("#") || /^[a-z][a-z0-9+.-]*:/i.test(href) ||
        href.startsWith("//")
      ) continue;
      const hash = href.indexOf("#");
      let fragment;
      let target;
      try {
        fragment = decodeURIComponent(href.slice(hash + 1));
        const filename = decodeURIComponent(
          href.slice(0, hash).split("?", 1)[0],
        );
        if (!fragment) continue; // A bare # links to the document top.
        if (
          filename && !/^ADR-[0-9]{3}[a-z]?-.+\.md$/i.test(basename(filename))
        ) continue;
        target = filename ? resolve(dirname(path), filename) : path;
      } catch {
        errors.push(`${path}: malformed fragment link ${JSON.stringify(href)}`);
        continue;
      }
      checked++;
      if (!documents.has(target)) {
        errors.push(
          `${path}: fragment target file is absent: ${JSON.stringify(href)}`,
        );
      } else if (!read(target).anchors.has(fragment)) {
        errors.push(`${path}: missing anchor #${fragment} in ${target}`);
      }
    }
  }
  return { errors, checked };
}

async function* markdownFiles(directory) {
  let entries;
  try {
    entries = Deno.readDir(directory);
    for await (const entry of entries) {
      const path = join(directory, entry.name);
      if (entry.isDirectory) yield* markdownFiles(path);
      else if (entry.isFile && path.endsWith(".md")) yield path;
    }
  } catch (error) {
    if (!(error instanceof Deno.errors.NotFound)) throw error;
  }
}

export async function repositoryDocuments(root) {
  const documents = new Map();
  for await (const path of markdownFiles(join(root, "docs"))) {
    documents.set(path, await Deno.readTextFile(path));
  }
  for await (const path of markdownFiles(join(root, "crates"))) {
    const parts = relative(join(root, "crates"), path).split(sep);
    if (
      parts.slice(0, -1).includes("docs") ||
      /^design.*\.md$/.test(basename(path))
    ) {
      documents.set(path, await Deno.readTextFile(path));
    }
  }
  return documents;
}

if (import.meta.main) {
  // A blind checker must fail before it can print a clean repository result.
  runSelfTests(checkDocuments);
  if (Deno.args[0] === "--self-test") {
    console.log("ADR anchor self-test: positive and must-fail fixtures OK");
  } else {
    const root = resolve(Deno.args[0] ?? ".");
    const documents = await repositoryDocuments(root);
    const { errors, checked } = checkDocuments(documents);
    for (const error of errors) console.error(error.replaceAll(root + sep, ""));
    if (errors.length) {
      console.error(`ADR anchor lint: ${errors.length} issue(s)`);
      Deno.exit(1);
    }
    console.log(
      `ADR anchor lint: ${checked} fragment(s) OK; must-fail fixtures verified`,
    );
  }
}

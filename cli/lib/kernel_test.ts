import { assertEquals } from "@std/assert";
import { dirname, fromFileUrl, join } from "@std/path";
import { kkernelPath } from "./kernel.ts";

interface Fixture {
  root: string;
  here: string;
  moduleRoot: string;
  repoRoot: string;
  exe: string;
  fileInfo: Deno.FileInfo;
  directoryInfo: Deno.FileInfo;
}

async function withFixture(run: (fixture: Fixture) => void): Promise<void> {
  const root = await Deno.makeTempDir({ prefix: "khive_kernel_path_" });
  try {
    const placeholder = join(root, "empty-placeholder");
    await Deno.writeTextFile(placeholder, "");
    const here = dirname(fromFileUrl(new URL("./kernel.ts", import.meta.url)));
    run({
      root,
      here,
      moduleRoot: dirname(here),
      repoRoot: join(root, "explicit-repo"),
      exe: Deno.build.os === "windows" ? "kkernel.exe" : "kkernel",
      fileInfo: Deno.statSync(placeholder),
      directoryInfo: Deno.statSync(root),
    });
  } finally {
    await Deno.remove(root, { recursive: true });
  }
}

function candidate(root: string, profile: "release" | "debug", exe: string): string {
  return join(root, "crates", "target", profile, exe);
}

function virtualFiles(fixture: Fixture, binaries: string[]): Map<string, Deno.FileInfo> {
  const files = new Map<string, Deno.FileInfo>();
  files.set(join(fixture.moduleRoot, "crates"), fixture.directoryInfo);
  for (const binary of binaries) files.set(binary, fixture.fileInfo);

  // A restored legacy resolver must observe an available retired package on every OS.
  const retired = join(fixture.here, "node_modules");
  files.set(retired, fixture.directoryInfo);
  for (
    const key of new Set([
      "darwin-arm64",
      "darwin-x64",
      "linux-x64-gnu",
      "linux-x64-musl",
      "linux-arm64",
      "win32-x64",
      `${Deno.build.os}-${Deno.build.arch}`,
    ])
  ) {
    files.set(join(retired, `khive-kernel-${key}`, "bin", fixture.exe), fixture.fileInfo);
  }
  return files;
}

interface SavedProperty {
  target: object;
  key: string;
  descriptor: PropertyDescriptor | undefined;
}

type ResolverResult = { kind: "returned"; value: string } | { kind: "threw"; error: unknown };

interface Observation {
  result: ResolverResult;
  paths: string[];
  envKeys: string[];
  commandCalls: number;
  readDirCalls: number;
  descriptors: { before: PropertyDescriptor | undefined; after: PropertyDescriptor | undefined }[];
}

function observe(
  files: Map<string, Deno.FileInfo>,
  override: string | undefined,
  repoRoot?: string,
): Observation {
  const paths: string[] = [];
  const envKeys: string[] = [];
  let commandCalls = 0;
  let readDirCalls = 0;
  let result: ResolverResult | undefined;
  const installed: SavedProperty[] = [];
  const definitions: { target: object; key: string; value: unknown }[] = [
    {
      target: Deno.env,
      key: "get",
      value: (key: string): string | undefined => {
        envKeys.push(key);
        if (key !== "KKERNEL_BINARY") throw new Error(`unexpected resolver env key: ${key}`);
        return override;
      },
    },
    {
      target: Deno,
      key: "statSync",
      value: (path: string | URL): Deno.FileInfo => {
        const name = path instanceof URL ? fromFileUrl(path) : path;
        paths.push(name);
        const info = files.get(name);
        if (info !== undefined) return info;
        throw new Error(`virtual fixture path absent: ${name}`);
      },
    },
    {
      target: Deno,
      key: "Command",
      value: class {
        constructor() {
          commandCalls++;
          throw new Error("resolver command probe forbidden");
        }
      },
    },
    {
      target: Deno,
      key: "readDirSync",
      value: () => {
        readDirCalls++;
        throw new Error("resolver directory scan forbidden");
      },
    },
  ];

  try {
    for (const definition of definitions) {
      const descriptor = Object.getOwnPropertyDescriptor(definition.target, definition.key);
      Object.defineProperty(definition.target, definition.key, {
        configurable: true,
        enumerable: descriptor?.enumerable ?? true,
        writable: true,
        value: definition.value,
      });
      installed.push({ target: definition.target, key: definition.key, descriptor });
    }
    // Only the real resolver call is caught: setup/restoration failures cannot be exhaustion.
    try {
      result = { kind: "returned", value: kkernelPath(repoRoot) };
    } catch (error) {
      result = { kind: "threw", error };
    }
  } finally {
    let restorationError: unknown;
    for (const property of [...installed].reverse()) {
      try {
        if (property.descriptor !== undefined) {
          Object.defineProperty(property.target, property.key, property.descriptor);
        } else if (!Reflect.deleteProperty(property.target, property.key)) {
          throw new Error(`cannot restore resolver probe property: ${property.key}`);
        }
      } catch (error) {
        restorationError ??= error;
      }
    }
    if (restorationError !== undefined) throw restorationError;
  }
  if (result === undefined) throw new Error("resolver fixture never invoked its subject");
  return {
    result,
    paths,
    envKeys,
    commandCalls,
    readDirCalls,
    descriptors: installed.map((property) => ({
      before: property.descriptor,
      after: Object.getOwnPropertyDescriptor(property.target, property.key),
    })),
  };
}

function assertIsolated(observation: Observation): void {
  for (const property of observation.descriptors) {
    assertEquals(property.after, property.before, "RESOLVER_DESCRIPTOR_MUST_BE_RESTORED");
  }
  const retiredProbes = observation.paths.filter((path) =>
    path === join(dirname(path), "node_modules") ||
    /(?:^|[\\/])node_modules[\\/]khive-kernel-[^\\/]+[\\/]bin[\\/][^\\/]+$/u.test(path)
  );
  assertEquals(retiredProbes, [], "RETIRED_PACKAGE_PROBE_FORBIDDEN");
  assertEquals(observation.commandCalls, 0, "RESOLVER_COMMAND_PROBE_FORBIDDEN");
  assertEquals(observation.readDirCalls, 0, "RESOLVER_DIRECTORY_SCAN_FORBIDDEN");
  assertEquals(observation.envKeys, ["KKERNEL_BINARY"]);
}

Deno.test("kkernelPath preserves valid and missing override behavior", async function overrides() {
  await withFixture((fixture) => {
    const release = candidate(fixture.repoRoot, "release", fixture.exe);
    const moduleRelease = candidate(fixture.moduleRoot, "release", fixture.exe);
    const override = join(fixture.root, "explicit-override");
    const files = virtualFiles(fixture, [override, release, moduleRelease]);
    const valid = observe(files, override, fixture.repoRoot);
    assertIsolated(valid);
    assertEquals(valid.result, { kind: "returned", value: override });
    assertEquals(valid.paths, [override], "valid override must return before fallback probes");

    const missing = join(fixture.root, "missing-override");
    const fallback = observe(files, missing, fixture.repoRoot);
    assertIsolated(fallback);
    assertEquals(fallback.result, { kind: "returned", value: release });
    assertEquals(fallback.paths[0], missing);
  });
});

Deno.test("kkernelPath preserves development candidate precedence", async function precedence() {
  await withFixture((fixture) => {
    const release = candidate(fixture.repoRoot, "release", fixture.exe);
    const debug = candidate(fixture.repoRoot, "debug", fixture.exe);
    const moduleRelease = candidate(fixture.moduleRoot, "release", fixture.exe);
    const moduleDebug = candidate(fixture.moduleRoot, "debug", fixture.exe);
    const scenarios = [
      {
        repo: fixture.repoRoot,
        files: [release, debug, moduleRelease, moduleDebug],
        want: release,
      },
      { repo: fixture.repoRoot, files: [debug, moduleRelease, moduleDebug], want: debug },
      { repo: undefined, files: [moduleRelease, moduleDebug], want: moduleRelease },
      { repo: fixture.repoRoot, files: [moduleDebug], want: moduleDebug },
    ];
    for (const scenario of scenarios) {
      const observation = observe(virtualFiles(fixture, scenario.files), undefined, scenario.repo);
      assertIsolated(observation);
      assertEquals(observation.result, { kind: "returned", value: scenario.want });
    }
  });
});

Deno.test("kkernelPath exhaustion has no retired platform probes", async function exhaustion() {
  await withFixture((fixture) => {
    const observation = observe(virtualFiles(fixture, []), undefined, fixture.repoRoot);
    assertIsolated(observation);
    const result = observation.result;
    if (result.kind !== "threw") throw new Error("expected actual resolver exhaustion");
    const message = result.error instanceof Error ? result.error.message : String(result.error);
    assertEquals(message.startsWith("kkernel binary not found.\nTried:\n"), true);
    const expected = [
      candidate(fixture.repoRoot, "release", fixture.exe),
      candidate(fixture.repoRoot, "debug", fixture.exe),
      candidate(fixture.moduleRoot, "release", fixture.exe),
      candidate(fixture.moduleRoot, "debug", fixture.exe),
    ];
    const positions = expected.map((path) => message.indexOf(path));
    assertEquals(
      positions.every((position) => position >= 0),
      true,
      "all candidates must be listed",
    );
    assertEquals(positions.every((position, i) => i === 0 || positions[i - 1] < position), true);
    assertEquals(message.includes("KKERNEL_BINARY"), true);
    assertEquals(message.includes("(npm install)"), false);
    assertEquals(message.includes("Supported platforms:"), false);
  });
});

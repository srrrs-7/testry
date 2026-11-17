// Benchmarks of testry (spec section 20). Runs inside the dev container:
//
//   make bench
//
// It builds synthetic repositories in a temporary directory, runs the
// release build of testry and the runners themselves against them, and
// writes every sample to docs/benchmarks/raw-<date>.jsonl together with the
// facts of the machine. Nothing here needs the network.
//
// What is compared (spec 20, "comparison targets"; the ones that exist
// without a container provider):
//   1. the runners started directly, as a project would start them;
//   2. `testry verify` on the same suites: the cost of orchestration;
//   3. `testry test` (affected) against the full run, for the same change.
// Planning is measured on its own for repositories of several sizes.
//
// Settings (environment variables):
//   BENCH_RUNS    recorded repetitions per measurement (default 30)
//   BENCH_WARMUP  discarded repetitions before them (default 3)
//   BENCH_SIZES   module counts of the planning repositories (default 100,1000,5000)
//   BENCH_TESTRY  the testry binary (default: the release build in the cargo target)

import {
  appendFileSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

const RUNS = Number(process.env.BENCH_RUNS ?? 30);
const WARMUP = Number(process.env.BENCH_WARMUP ?? 3);
const SIZES = (process.env.BENCH_SIZES ?? "100,1000,5000").split(",").map(Number);
const TESTRY =
  process.env.BENCH_TESTRY ?? join(process.env.CARGO_TARGET_DIR ?? "target", "release/testry");
const MODULES = process.env.TESTRY_RUNNER_FIXTURE_MODULES;
if (!MODULES) {
  throw new Error("TESTRY_RUNNER_FIXTURE_MODULES is not set: run this in the dev container (make bench)");
}

const repository = join(import.meta.dir, "..");
const date = new Date().toISOString().slice(0, 10);
const outputDirectory = join(repository, "docs/benchmarks");
mkdirSync(outputDirectory, { recursive: true });
const rawPath = join(outputDirectory, `raw-${date}.jsonl`);
writeFileSync(rawPath, "");

type Sample = {
  wall_ms: number;
  exit: number;
  max_rss_bytes: number;
  cpu_user_ms: number;
  cpu_system_ms: number;
  stdout: string;
  stderr: string;
};

/** Runs one command to completion and measures it. */
function run(argv: string[], cwd: string, env: Record<string, string> = {}): Sample {
  const started = performance.now();
  const child = Bun.spawnSync(argv, {
    cwd,
    env: { PATH: process.env.PATH ?? "", HOME: process.env.HOME ?? "", ...env },
    stdout: "pipe",
    stderr: "pipe",
  });
  const wall = performance.now() - started;
  const usage = child.resourceUsage;
  return {
    wall_ms: Math.round(wall * 1000) / 1000,
    exit: child.exitCode,
    max_rss_bytes: usage?.maxRSS ?? 0,
    cpu_user_ms: Number(usage?.cpuTime.user ?? 0n) / 1000,
    cpu_system_ms: Number(usage?.cpuTime.system ?? 0n) / 1000,
    stdout: child.stdout.toString(),
    stderr: child.stderr.toString(),
  };
}

function must(sample: Sample, expectedExit: number, what: string): Sample {
  if (sample.exit !== expectedExit) {
    throw new Error(`${what}: exit ${sample.exit}, expected ${expectedExit}\n${sample.stderr}\n${sample.stdout}`);
  }
  return sample;
}

function write(root: string, path: string, content: string) {
  const full = join(root, path);
  mkdirSync(dirname(full), { recursive: true });
  writeFileSync(full, content);
}

function git(root: string, ...args: string[]) {
  must(run(["git", "-C", root, ...args], root), 0, `git ${args.join(" ")}`);
}

function commit(root: string) {
  git(root, "init", "--quiet", "--initial-branch=main");
  git(root, "config", "user.name", "bench");
  git(root, "config", "user.email", "bench@example.invalid");
  git(root, "config", "commit.gpgsign", "false");
  git(root, "add", "--all");
  git(root, "commit", "--quiet", "--message", "bench");
}

/** Nearest-rank percentile of a sorted list. */
function percentile(sorted: number[], fraction: number): number {
  return sorted[Math.min(sorted.length - 1, Math.max(0, Math.ceil(fraction * sorted.length) - 1))];
}

type Summary = {
  scenario: string;
  measurement: string;
  size: number | null;
  runs: number;
  wall_ms: { min: number; p50: number; p95: number; max: number };
  max_rss_mib_p50: number;
  cpu_ms_p50: number;
  phases_ms_p50?: Record<string, number>;
  detail?: Record<string, number | string>;
};
const summaries: Summary[] = [];

/**
 * Measures `once` WARMUP + RUNS times. `once` returns the samples of one
 * repetition (several when the measurement is a sequence of commands) and
 * optionally facts about it.
 */
function measure(
  scenario: string,
  measurement: string,
  size: number | null,
  once: () => { samples: Sample[]; phases?: Record<string, number>; detail?: Record<string, number | string> },
) {
  for (let warm = 0; warm < WARMUP; warm++) once();
  const walls: number[] = [];
  const rss: number[] = [];
  const cpu: number[] = [];
  const phases: Record<string, number[]> = {};
  let detail: Record<string, number | string> | undefined;
  for (let index = 0; index < RUNS; index++) {
    const result = once();
    const wall = result.samples.reduce((total, sample) => total + sample.wall_ms, 0);
    const peak = Math.max(...result.samples.map((sample) => sample.max_rss_bytes));
    const cpuTime = result.samples.reduce(
      (total, sample) => total + sample.cpu_user_ms + sample.cpu_system_ms,
      0,
    );
    walls.push(wall);
    rss.push(peak);
    cpu.push(cpuTime);
    for (const [name, value] of Object.entries(result.phases ?? {})) (phases[name] ??= []).push(value);
    detail = result.detail;
    const line = {
      scenario,
      measurement,
      size,
      run: index + 1,
      wall_ms: Math.round(wall * 1000) / 1000,
      max_rss_bytes: peak,
      cpu_ms: Math.round(cpuTime * 1000) / 1000,
      phases_ms: result.phases ?? null,
      detail: result.detail ?? null,
    };
    appendFileSync(rawPath, JSON.stringify(line) + "\n");
  }
  const sorted = (values: number[]) => [...values].sort((left, right) => left - right);
  const sortedWalls = sorted(walls);
  const round = (value: number) => Math.round(value * 10) / 10;
  const summary: Summary = {
    scenario,
    measurement,
    size,
    runs: RUNS,
    wall_ms: {
      min: round(sortedWalls[0]),
      p50: round(percentile(sortedWalls, 0.5)),
      p95: round(percentile(sortedWalls, 0.95)),
      max: round(sortedWalls[sortedWalls.length - 1]),
    },
    max_rss_mib_p50: round(percentile(sorted(rss), 0.5) / 1024 / 1024),
    cpu_ms_p50: round(percentile(sorted(cpu), 0.5)),
    detail,
  };
  if (Object.keys(phases).length > 0) {
    summary.phases_ms_p50 = Object.fromEntries(
      Object.entries(phases).map(([name, values]) => [name, percentile(sorted(values), 0.5)]),
    );
  }
  summaries.push(summary);
  console.error(
    `${scenario} | ${measurement}${size === null ? "" : ` | ${size}`} | p50 ${summary.wall_ms.p50} ms, p95 ${summary.wall_ms.p95} ms`,
  );
}

/** `planning took 14 ms (snapshot 6 ms, inventory 0 ms, analysis 6 ms, selection 0 ms)` */
function planningPhases(stderr: string): Record<string, number> {
  const line = stderr.split("\n").find((candidate) => candidate.startsWith("planning took"));
  if (!line) throw new Error(`no planning timings in:\n${stderr}`);
  const phases: Record<string, number> = { total: Number(line.match(/took (\d+) ms/)?.[1]) };
  for (const match of line.matchAll(/(\w+) (\d+) ms/g)) {
    if (match[1] !== "took") phases[match[1]] = Number(match[2]);
  }
  return phases;
}

const stateHome = mkdtempSync(join(tmpdir(), "testry-bench-state-"));
const testryEnv = { TESTRY_STATE_HOME: stateHome };
function testry(root: string, ...args: string[]): Sample {
  return run([TESTRY, ...args], root, testryEnv);
}

// ------------------------------------------------------------- planning --

const CHAIN = 10;

/** `modules` source files in import chains of ten, one test per chain. */
function planningRepository(modules: number): string {
  const root = mkdtempSync(join(tmpdir(), `testry-bench-plan-${modules}-`));
  write(
    root,
    "testry.toml",
    `schema_version = 1
[workspace]
exclude = ["**/node_modules/**"]
[analysis]
global_inputs = ["**/package.json"]
[execution]
default_environment = "local"
cpu_slots = 2
memory_budget_mib = 2048
[environments.local]
provider = "host"
[[suites]]
id = "unit"
runner = "bun"
command = ["bun", "test"]
include = ["src/**/*.test.ts"]
inputs = ["src/**"]
selection = "file_graph"
[suites.resources]
cpu_slots = 1
memory_mib = 512
`,
  );
  write(root, "package.json", `{ "name": "bench", "private": true, "type": "module" }\n`);
  const chains = Math.ceil(modules / CHAIN);
  for (let chain = 0; chain < chains; chain++) {
    for (let link = 0; link < CHAIN; link++) {
      const next =
        link + 1 < CHAIN
          ? `import { value as next } from "./m${link + 1}";\nexport const value = (): number => next() + 1;\n`
          : `export const value = (): number => 1;\n`;
      write(root, `src/c${chain}/m${link}.ts`, next);
    }
    write(
      root,
      `src/c${chain}/chain.test.ts`,
      `import { expect, test } from "bun:test";\nimport { value } from "./m0";\ntest("chain ${chain}", () => { expect(value()).toBe(${CHAIN}); });\n`,
    );
  }
  commit(root);
  return root;
}

for (const size of SIZES) {
  const root = planningRepository(size);
  const files = Math.ceil(size / CHAIN) * (CHAIN + 1);
  const plan = (...args: string[]) => () => {
    const sample = must(testry(root, "plan", "--format", "json", ...args), 0, "plan");
    const document = JSON.parse(sample.stdout);
    const selected = document.suites[0].effective_selection;
    return {
      samples: [sample],
      phases: planningPhases(sample.stderr),
      detail: {
        source_files: files,
        selection: selected.kind,
        selected_test_files: selected.kind === "files" ? selected.files.length : -1,
        fallbacks: document.fallbacks.length,
      },
    };
  };
  measure("planning", "plan --mode full", size, plan("--mode", "full"));
  measure("planning", "plan (affected, nothing changed)", size, plan());
  // The deepest module of one chain changes: one test file is affected.
  writeFileSync(join(root, `src/c0/m${CHAIN - 1}.ts`), `export const value = (): number => 1;\nexport const changed = true;\n`);
  measure("planning", "plan (affected, one module changed)", size, plan());
  rmSync(root, { recursive: true, force: true });
}

// ------------------------------------------------------------ execution --

const BUN_FILES = 40;
const VITEST_FILES = 20;

function executionConfig(parallel: number): string {
  return `schema_version = 1
[workspace]
exclude = ["**/node_modules/**", "node_modules"]
[analysis]
global_inputs = ["**/package.json"]
[execution]
default_environment = "local"
cpu_slots = 4
memory_budget_mib = 4096
max_parallel_suites = ${parallel}
[environments.local]
provider = "host"
[[suites]]
id = "api-unit"
runner = "bun"
cwd = "apps/api"
command = ["bun", "test"]
include = ["apps/api/**/*.test.ts"]
inputs = ["apps/api/**"]
selection = "file_graph"
[suites.resources]
cpu_slots = 1
memory_mib = 512
[[suites]]
id = "web-unit"
runner = "vitest"
cwd = "apps/web"
command = ["node", "../../node_modules/vitest/vitest.mjs", "run"]
include = ["apps/web/**/*.test.ts"]
inputs = ["apps/web/**"]
selection = "file_graph"
workers = 1
[suites.resources]
cpu_slots = 1
memory_mib = 768
`;
}

const execution = mkdtempSync(join(tmpdir(), "testry-bench-run-"));
write(execution, "testry.toml", executionConfig(2));
write(execution, "testry.serial.toml", executionConfig(1));
write(execution, ".gitignore", "node_modules\n");
write(
  execution,
  "package.json",
  `{ "name": "bench", "private": true, "type": "module", "devDependencies": { "vitest": "5.1.0" } }\n`,
);
for (let index = 0; index < BUN_FILES; index++) {
  write(execution, `apps/api/src/f${index}.ts`, `export const f${index} = (input: number): number => input + ${index};\n`);
  write(
    execution,
    `apps/api/src/f${index}.test.ts`,
    `import { expect, test } from "bun:test";\nimport { f${index} } from "./f${index}";\n` +
      [0, 1, 2, 3, 4].map((n) => `test("case ${n}", () => { expect(f${index}(${n})).toBe(${index + n}); });`).join("\n") +
      "\n",
  );
}
for (let index = 0; index < VITEST_FILES; index++) {
  write(execution, `apps/web/src/g${index}.ts`, `export const g${index} = (input: number): number => input * ${index + 1};\n`);
  write(
    execution,
    `apps/web/src/g${index}.test.ts`,
    `import { expect, test } from "vitest";\nimport { g${index} } from "./g${index}";\n` +
      [1, 2, 3, 4, 5].map((n) => `test("case ${n}", () => { expect(g${index}(${n})).toBe(${(index + 1) * n}); });`).join("\n") +
      "\n",
  );
}
symlinkSync(MODULES, join(execution, "node_modules"));
commit(execution);
// One approval exists per workspace, so the configuration in use is approved
// right before it is measured.
const approve = (config: string) =>
  must(testry(execution, "--config", config, "trust", "--approve"), 0, `trust ${config}`);

const direct = (argv: string[], cwd: string) => must(run(argv, join(execution, cwd)), 0, argv.join(" "));
const vitest = ["node", "../../node_modules/vitest/vitest.mjs"];
const resultOf = (sample: Sample) => JSON.parse(sample.stdout);
const suiteTimes = (result: any): Record<string, number> =>
  Object.fromEntries(result.suites.map((suite: any) => [`suite:${suite.suite_id}`, suite.duration_ms]));

// 1. The runners themselves, one after the other. The suite reserves one
//    CPU slot for Vitest and testry passes that on as --maxWorkers=1, so the
//    like-for-like direct run uses the same worker count. The run with
//    Vitest's own default (one worker per core) is recorded as well.
measure("execution", "runners directly, full (bun test; vitest run --maxWorkers=1)", null, () => ({
  samples: [direct(["bun", "test"], "apps/api"), direct([...vitest, "run", "--maxWorkers=1"], "apps/web")],
  detail: { test_files: BUN_FILES + VITEST_FILES },
}));
measure("execution", "runners directly, full (bun test; vitest run, default workers)", null, () => ({
  samples: [direct(["bun", "test"], "apps/api"), direct([...vitest, "run"], "apps/web")],
  detail: { test_files: BUN_FILES + VITEST_FILES },
}));
// What testry adds before the first suite: it asks every runner for its version.
measure("execution", "version probes directly (bun --version; vitest --version)", null, () => ({
  samples: [direct(["bun", "--version"], "apps/api"), direct([...vitest, "--version"], "apps/web")],
}));

// 2. The same suites through testry: one suite at a time, then two at a time.
const verify = (config: string) => () => {
  const sample = must(testry(execution, "--config", config, "verify", "--format", "json"), 0, "verify");
  const result = resultOf(sample);
  return {
    samples: [sample],
    phases: { ...result.metrics, ...suiteTimes(result) },
    detail: {
      test_files: result.suites.reduce((total: number, suite: any) => total + suite.executed_files.length, 0),
      tests_passed: result.suites.reduce((total: number, suite: any) => total + suite.counts.passed, 0),
      assurance: result.assurance,
    },
  };
};
approve("testry.serial.toml");
measure("execution", "testry verify, one suite at a time", null, verify("testry.serial.toml"));
approve("testry.toml");
measure("execution", "testry verify, two suites at a time", null, verify("testry.toml"));

// 3. One source file of the Bun suite changes.
writeFileSync(
  join(execution, "apps/api/src/f7.ts"),
  `export const f7 = (input: number): number => input + 7;\nexport const changed = true;\n`,
);
measure("execution", "runner directly, the one affected file (bun test ./src/f7.test.ts)", null, () => ({
  samples: [direct(["bun", "test", "./src/f7.test.ts"], "apps/api")],
  detail: { test_files: 1 },
}));
measure("execution", "testry test (affected, one module changed)", null, () => {
  const sample = must(testry(execution, "test", "--format", "json"), 0, "test");
  const result = resultOf(sample);
  return {
    samples: [sample],
    phases: { ...result.metrics, ...suiteTimes(result) },
    detail: {
      test_files: result.suites.reduce((total: number, suite: any) => total + suite.executed_files.length, 0),
      suites_run: result.suites.length,
      assurance: result.assurance,
    },
  };
});
measure("execution", "testry verify with the same change (full)", null, verify("testry.toml"));
rmSync(execution, { recursive: true, force: true });
rmSync(stateHome, { recursive: true, force: true });

// ---------------------------------------------------------- environment --

// With the environment of this script: the toolchain is located through it.
const versionOf = (argv: string[]) => {
  const child = Bun.spawnSync(argv, { cwd: repository, env: process.env, stdout: "pipe", stderr: "pipe" });
  if (child.exitCode !== 0) return `unknown (exit ${child.exitCode})`;
  return child.stdout.toString().trim().split("\n")[0];
};
const meminfo = readFileSync("/proc/meminfo", "utf8");
const cpuinfo = readFileSync("/proc/cpuinfo", "utf8");
const environment = {
  date,
  runs_per_measurement: RUNS,
  warmup_runs: WARMUP,
  planning_sizes: SIZES,
  testry_binary: TESTRY.includes("/release/") ? "release build" : TESTRY,
  testry: versionOf([TESTRY, "--version"]),
  rustc: versionOf(["rustc", "--version"]),
  bun: versionOf(["bun", "--version"]),
  node: versionOf(["node", "--version"]),
  vitest: JSON.parse(readFileSync(join(MODULES, "vitest/package.json"), "utf8")).version,
  git: versionOf(["git", "--version"]),
  kernel: versionOf(["uname", "-srm"]),
  cpus_online: (cpuinfo.match(/^processor\s*:/gm) ?? []).length,
  memory_total_kib: Number(meminfo.match(/MemTotal:\s+(\d+)/)?.[1]),
  container: "dev image of this repository (compose service `bench`, no network)",
  caches:
    "file system cache warm (warm-up runs precede every measurement); testry keeps no cache of its own between runs; no warm pool exists",
};
writeFileSync(join(outputDirectory, `environment-${date}.json`), JSON.stringify(environment, null, 2) + "\n");
writeFileSync(join(outputDirectory, `summary-${date}.json`), JSON.stringify(summaries, null, 2) + "\n");

console.log(`| scenario | measurement | size | wall p50 (ms) | wall p95 (ms) | min–max (ms) | peak RSS p50 (MiB) | CPU p50 (ms) |`);
console.log(`|---|---|---|---|---|---|---|---|`);
for (const summary of summaries) {
  console.log(
    `| ${summary.scenario} | ${summary.measurement} | ${summary.size ?? ""} | ${summary.wall_ms.p50} | ${summary.wall_ms.p95} | ${summary.wall_ms.min}–${summary.wall_ms.max} | ${summary.max_rss_mib_p50} | ${summary.cpu_ms_p50} |`,
  );
}
console.error(`raw samples: ${rawPath}`);

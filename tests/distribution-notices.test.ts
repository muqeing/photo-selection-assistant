import { execFile } from "node:child_process";
import {
  chmod,
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { delimiter, resolve } from "node:path";
import { promisify } from "node:util";
import { describe, expect, it } from "vitest";

const execFileAsync = promisify(execFile);
const root = resolve(import.meta.dirname, "..");

async function filesUnder(directory: string): Promise<string[]> {
  const entries = await readdir(directory, { withFileTypes: true });
  const files: string[] = [];
  for (const entry of entries) {
    const path = resolve(directory, entry.name);
    if (entry.isDirectory()) files.push(...await filesUnder(path));
    else files.push(path);
  }
  return files;
}

describe("distribution notice preparation", () => {
  it.skipIf(process.platform === "win32")("collects Rust and Node notices without exposing fixture paths", async () => {
    const fixture = await mkdtemp(resolve(tmpdir(), "distribution-notices-"));
    try {
      await mkdir(resolve(fixture, "scripts"), { recursive: true });
      await mkdir(resolve(fixture, "src-tauri/crates/example/licenses"), { recursive: true });
      await mkdir(resolve(fixture, "node_modules/runtime-a"), { recursive: true });
      await mkdir(resolve(fixture, "node_modules/missing"), { recursive: true });
      await mkdir(resolve(fixture, "node_modules/.pnpm/node-worker@1.2.3/node_modules/node-worker"), { recursive: true });
      await mkdir(resolve(fixture, "bin"), { recursive: true });
      await copyFile(
        resolve(root, "scripts/prepare-distribution-notices.mjs"),
        resolve(fixture, "scripts/prepare-distribution-notices.mjs"),
      );
      await writeFile(resolve(fixture, "package.json"), JSON.stringify({ version: "9.9.9" }));
      await writeFile(resolve(fixture, "LICENSE"), "Fixture application copyright\n");
      await writeFile(resolve(fixture, "THIRD_PARTY_NOTICES.md"), "Fixture application notices\n");
      await writeFile(resolve(fixture, "src-tauri/Cargo.toml"), "[workspace]\nmembers = []\n");
      await writeFile(
        resolve(fixture, "src-tauri/crates/example/Cargo.toml"),
        "[package]\nname = \"example-crate\"\nversion = \"0.1.0\"\n",
      );
      await writeFile(resolve(fixture, "src-tauri/crates/example/licenses/LICENSE-MIT"), "Example crate copyright\n");
      await writeFile(resolve(fixture, "node_modules/runtime-a/package.json"), JSON.stringify({
        name: "runtime-a",
        version: "1.0.0",
        license: "MIT",
        repository: "https://example.invalid/runtime-a",
      }));
      await writeFile(resolve(fixture, "node_modules/runtime-a/LICENSE"), "Runtime A copyright\n");
      await writeFile(resolve(fixture, "node_modules/.pnpm/node-worker@1.2.3/node_modules/node-worker/package.json"), JSON.stringify({
        name: "node-worker",
        version: "1.2.3",
        license: "Apache-2.0",
      }));
      await writeFile(
        resolve(fixture, "node_modules/.pnpm/node-worker@1.2.3/node_modules/node-worker/worker.min.js.LICENSE.txt"),
        "Worker sidecar copyright\n",
      );
      await writeFile(resolve(fixture, "node_modules/missing/package.json"), JSON.stringify({
        name: "missing",
        version: "2.0.0",
        license: "MIT",
      }));

      const cargoStub = `#!/usr/bin/env node
import { resolve } from "node:path";
const root = process.cwd();
console.log(JSON.stringify({ packages: [
  { name: "fixture-app", version: "9.9.9", license: "MIT", authors: ["Fixture"], repository: "https://example.invalid/app", manifest_path: resolve(root, "src-tauri/Cargo.toml") },
  { name: "example-crate", version: "0.1.0", license: "MIT", authors: ["Fixture"], repository: "https://example.invalid/example-crate", manifest_path: resolve(root, "src-tauri/crates/example/Cargo.toml") },
] }));
`;
      const cargo = resolve(fixture, "bin", process.platform === "win32" ? "cargo.cmd" : "cargo");
      if (process.platform === "win32") {
        await writeFile(cargo, `@echo off\r\n"${process.execPath}" "%~dp0\\cargo-stub.mjs" %*\r\n`);
        await writeFile(resolve(fixture, "bin/cargo-stub.mjs"), cargoStub.replace(/^#!.*\n/u, ""));
      } else {
        await writeFile(cargo, cargoStub);
        await chmod(cargo, 0o755);
      }

      const result = await execFileAsync(process.execPath, [resolve(fixture, "scripts/prepare-distribution-notices.mjs")], {
        cwd: fixture,
        env: { ...process.env, PATH: `${resolve(fixture, "bin")}${delimiter}${process.env.PATH ?? ""}` },
      });
      expect(result.stdout).toContain("1 Rust packages, 3 Node packages");
      expect(result.stdout).not.toContain("Runtime A copyright");

      const output = resolve(fixture, "public/third-party-licenses");
      const inventory = JSON.parse(await readFile(resolve(output, "inventory.json"), "utf8"));
      expect(inventory.summary.packageCount).toBe(4);
      expect(inventory.packages.find((pkg: { name: string }) => pkg.name === "node-worker")).toMatchObject({
        licenseTextFound: true,
        sourceUrl: "https://www.npmjs.com/package/node-worker/v/1.2.3",
        sourceDownloadUrl: "https://www.npmjs.com/package/node-worker/v/1.2.3",
      });
      expect(inventory.packages.find((pkg: { name: string }) => pkg.name === "missing")).toMatchObject({
        licenseTextFound: false,
        licenseTextStatus: "not-found",
      });
      expect(await readFile(resolve(output, "texts/application/LICENSE"), "utf8")).toContain("application copyright");
      expect(await readFile(resolve(output, "texts/node-node-worker-1.2.3/worker.min.js.LICENSE.txt"), "utf8")).toContain("Worker sidecar");

      for (const path of await filesUnder(output)) {
        const content = await readFile(path, "utf8");
        expect(content).not.toContain(fixture);
      }
    } finally {
      await rm(fixture, { recursive: true, force: true });
    }
  });
});

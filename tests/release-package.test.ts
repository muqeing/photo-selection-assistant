import { execFile } from "node:child_process";
import {
  chmod,
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { delimiter, resolve } from "node:path";
import { promisify } from "node:util";
import { describe, expect, it } from "vitest";

const execFileAsync = promisify(execFile);
const root = resolve(import.meta.dirname, "..");

async function git(cwd: string, ...args: string[]) {
  return execFileAsync("git", args, { cwd });
}

const validReleaseConfig = {
  bundle: {
    macOS: {
      minimumSystemVersion: "11.0",
      signingIdentity: "-",
    },
    windows: { wix: { language: "zh-CN" } },
  },
};

async function releaseFixture(
  tauriConfig: Record<string, unknown> = validReleaseConfig,
) {
  const fixture = await mkdtemp(resolve(tmpdir(), "photo-release-wrapper-"));
  await mkdir(resolve(fixture, "scripts"), { recursive: true });
  await mkdir(resolve(fixture, "src-tauri"), { recursive: true });
  await mkdir(resolve(fixture, "bin"), { recursive: true });
  await copyFile(
    resolve(root, "scripts/package-release.mjs"),
    resolve(fixture, "scripts/package-release.mjs"),
  );
  await writeFile(
    resolve(fixture, "package.json"),
    JSON.stringify({ version: "0.1.22" }),
  );
  await writeFile(
    resolve(fixture, "src-tauri/tauri.conf.json"),
    JSON.stringify(tauriConfig),
  );
  await copyFile(
    resolve(root, "src-tauri/Cargo.toml"),
    resolve(fixture, "src-tauri/Cargo.toml"),
  );
  await writeFile(resolve(fixture, ".gitignore"), "src-tauri/target/\n");
  const stub = `#!/usr/bin/env node
import { execFileSync } from "node:child_process";
import { appendFileSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
const root = process.cwd();
const mode = process.env.RELEASE_STUB_MODE ?? "success";
const bundle = resolve(root, "src-tauri/target/release/bundle");
const windows = process.argv.includes("nsis,msi");
const artifacts = windows
  ? [
      resolve(bundle, "nsis", "照片筛选助手_0.1.22_x64-setup.exe"),
      resolve(bundle, "msi", "照片筛选助手_0.1.22_x64_zh-CN.msi"),
    ]
  : [resolve(bundle, "dmg", "照片筛选助手_0.1.22_aarch64.dmg")];
if (mode === "fail") process.exit(23);
if (mode === "dirty") appendFileSync(resolve(root, "package.json"), " ");
if (mode === "head") {
  execFileSync("git", ["commit", "--allow-empty", "-m", "changed during build"], { cwd: root });
}
if (mode === "tauri-normalize") {
  const manifestPath = resolve(root, "src-tauri/Cargo.toml");
  const manifest = readFileSync(manifestPath, "utf8");
  const normalized = manifest
    .replace(/^(tauri-build = \\{ version = "[^"]+") \\}$/mu, "$1, features = [] }")
    .replace(/^(tauri = \\{ version = "[^"]+") \\}$/mu, "$1, features = [] }");
  if (normalized !== manifest) writeFileSync(manifestPath, normalized);
}
if (
  mode === "success"
  || mode === "dirty"
  || mode === "head"
  || mode === "partial"
  || mode === "tauri-normalize"
) {
  const selected = mode === "partial" ? artifacts.slice(0, 1) : artifacts;
  for (const artifact of selected) {
    mkdirSync(resolve(artifact, ".."), { recursive: true });
    writeFileSync(artifact, "fresh artifact");
  }
}
`;
  const pnpm = resolve(fixture, "bin", process.platform === "win32" ? "pnpm.cmd" : "pnpm");
  await writeFile(
    pnpm,
    process.platform === "win32"
      ? `@echo off\r\n"${process.execPath}" "%~dp0\\pnpm-stub.mjs" %*\r\n`
      : stub,
  );
  if (process.platform === "win32") {
    await writeFile(resolve(fixture, "bin/pnpm-stub.mjs"), stub.replace(/^#!.*\n/u, ""));
  } else {
    await chmod(pnpm, 0o755);
  }
  await git(fixture, "init");
  await git(fixture, "config", "user.email", "test@example.invalid");
  await git(fixture, "config", "user.name", "Release Test");
  await git(fixture, "add", ".");
  await git(fixture, "commit", "-m", "fixture");
  return fixture;
}

async function runPackage(
  fixture: string,
  mode: string,
  platform: "macos" | "windows" = "macos",
) {
  return execFileAsync(
    process.execPath,
    [resolve(fixture, "scripts/package-release.mjs"), platform],
    {
      cwd: fixture,
      env: {
        ...process.env,
        PATH: `${resolve(fixture, "bin")}${delimiter}${process.env.PATH ?? ""}`,
        RELEASE_STUB_MODE: mode,
      },
    },
  );
}

const artifactPath = (fixture: string) =>
  resolve(
    fixture,
    "src-tauri/target/release/bundle/dmg/照片筛选助手_0.1.22_aarch64.dmg",
  );
const windowsArtifactPaths = (fixture: string) => [
  resolve(
    fixture,
    "src-tauri/target/release/bundle/nsis/照片筛选助手_0.1.22_x64-setup.exe",
  ),
  resolve(
    fixture,
    "src-tauri/target/release/bundle/msi/照片筛选助手_0.1.22_x64_zh-CN.msi",
  ),
];

describe("release package wrapper", () => {
  it("refuses a macOS release without a whole-bundle signing identity", async () => {
    const fixture = await releaseFixture({
      bundle: {
        macOS: { minimumSystemVersion: "11.0" },
        windows: { wix: { language: "zh-CN" } },
      },
    });
    try {
      await expect(runPackage(fixture, "success")).rejects.toMatchObject({
        stderr: expect.stringContaining("macOS signing identity"),
      });
      await expect(readFile(artifactPath(fixture))).rejects.toMatchObject({
        code: "ENOENT",
      });
    } finally {
      await rm(fixture, { recursive: true, force: true });
    }
  });

  it("refuses an Apple Silicon release configured below macOS 11", async () => {
    const fixture = await releaseFixture({
      bundle: {
        macOS: {
          minimumSystemVersion: "10.13",
          signingIdentity: "-",
        },
        windows: { wix: { language: "zh-CN" } },
      },
    });
    try {
      await expect(runPackage(fixture, "success")).rejects.toMatchObject({
        stderr: expect.stringContaining("macOS 11.0 or newer"),
      });
      await expect(readFile(artifactPath(fixture))).rejects.toMatchObject({
        code: "ENOENT",
      });
    } finally {
      await rm(fixture, { recursive: true, force: true });
    }
  });

  it("builds from a clean captured HEAD and writes verified provenance", async () => {
    const fixture = await releaseFixture();
    try {
      const head = (await git(fixture, "rev-parse", "HEAD")).stdout.trim();
      await runPackage(fixture, "success");
      const provenance = JSON.parse(
        await readFile(`${artifactPath(fixture)}.provenance.json`, "utf8"),
      );
      expect(provenance).toMatchObject({
        schemaVersion: 1,
        filename: "照片筛选助手_0.1.22_aarch64.dmg",
        sourceCommit: head,
      });
      expect(provenance.sizeBytes).toBeGreaterThan(0);
      expect(provenance.sha256).toMatch(/^[0-9a-f]{64}$/u);
    } finally {
      await rm(fixture, { recursive: true, force: true });
    }
  });

  it("deletes an old same-version artifact and refuses to stamp when build creates none", async () => {
    const fixture = await releaseFixture();
    try {
      await mkdir(resolve(artifactPath(fixture), ".."), { recursive: true });
      await writeFile(artifactPath(fixture), "old artifact");
      await writeFile(`${artifactPath(fixture)}.provenance.json`, "{}");

      await expect(runPackage(fixture, "none")).rejects.toMatchObject({
        stderr: expect.stringContaining("missing after build"),
      });
      await expect(readFile(artifactPath(fixture))).rejects.toMatchObject({
        code: "ENOENT",
      });
      await expect(
        readFile(`${artifactPath(fixture)}.provenance.json`),
      ).rejects.toMatchObject({ code: "ENOENT" });
    } finally {
      await rm(fixture, { recursive: true, force: true });
    }
  });

  it("writes no sidecar when the build fails", async () => {
    const fixture = await releaseFixture();
    try {
      await expect(runPackage(fixture, "fail")).rejects.toBeDefined();
      await expect(
        readFile(`${artifactPath(fixture)}.provenance.json`),
      ).rejects.toMatchObject({ code: "ENOENT" });
    } finally {
      await rm(fixture, { recursive: true, force: true });
    }
  });

  it("keeps Tauri manifest normalization a no-op during the wrapped build", async () => {
    const fixture = await releaseFixture();
    try {
      await expect(
        runPackage(fixture, "tauri-normalize"),
      ).resolves.toBeDefined();
      expect((await git(fixture, "status", "--porcelain=v1")).stdout).toBe("");
      await expect(
        readFile(`${artifactPath(fixture)}.provenance.json`, "utf8"),
      ).resolves.toContain('"sourceCommit"');
    } finally {
      await rm(fixture, { recursive: true, force: true });
    }
  });

  it("writes no Windows sidecar when only one of the two required artifacts is rebuilt", async () => {
    const fixture = await releaseFixture();
    try {
      await expect(
        runPackage(fixture, "partial", "windows"),
      ).rejects.toMatchObject({
        stderr: expect.stringContaining("missing after build"),
      });
      for (const artifact of windowsArtifactPaths(fixture)) {
        await expect(
          readFile(`${artifact}.provenance.json`),
        ).rejects.toMatchObject({ code: "ENOENT" });
      }
    } finally {
      await rm(fixture, { recursive: true, force: true });
    }
  });

  it.each(["dirty", "head"])(
    "rejects a %s source tree change made during the build",
    async mode => {
      const fixture = await releaseFixture();
      try {
        await expect(runPackage(fixture, mode)).rejects.toMatchObject({
          stderr: expect.stringContaining("changed during release build"),
        });
        await expect(
          readFile(`${artifactPath(fixture)}.provenance.json`),
        ).rejects.toMatchObject({ code: "ENOENT" });
      } finally {
        await rm(fixture, { recursive: true, force: true });
      }
    },
  );
});

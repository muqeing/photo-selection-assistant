import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { resolve } from "node:path";
import { promisify } from "node:util";
import { describe, expect, it } from "vitest";

const execFileAsync = promisify(execFile);
const root = resolve(import.meta.dirname, "..");

async function writeDmgFixture(
  bundleDirectory: string,
  sourceCommit: string,
  overrides: Record<string, unknown> = {},
) {
  const filename = "照片筛选助手_0.1.24_aarch64.dmg";
  const dmgDirectory = resolve(bundleDirectory, "dmg");
  const artifact = resolve(dmgDirectory, filename);
  const bytes = Buffer.from("verified artifact bytes");
  await mkdir(dmgDirectory, { recursive: true });
  await writeFile(artifact, bytes);
  await writeFile(
    `${artifact}.provenance.json`,
    `${JSON.stringify({
      schemaVersion: 1,
      filename,
      sizeBytes: bytes.byteLength,
      sha256: createHash("sha256").update(bytes).digest("hex"),
      sourceCommit,
      ...overrides,
    })}\n`,
  );
  return artifact;
}

describe("release metadata", () => {
  it("writes no-argument metadata under the bundle directory without changing history", async () => {
    const fixtureRoot = await mkdtemp(resolve(tmpdir(), "photo-release-default-output-"));
    const bundleDirectory = resolve(fixtureRoot, "bundle");
    const tauriConfig = resolve(fixtureRoot, "tauri.conf.json");
    const historicalPath = resolve(root, "docs/release-artifacts.json");
    const historicalBefore = await readFile(historicalPath, "utf8");

    try {
      await writeFile(
        tauriConfig,
        JSON.stringify({ bundle: { windows: { wix: { language: "zh-CN" } } } }),
      );
      await execFileAsync(
        process.execPath,
        [resolve(root, "scripts/generate-release-metadata.mjs")],
        {
          cwd: root,
          env: {
            ...process.env,
            RELEASE_BUNDLE_DIRECTORY: bundleDirectory,
            RELEASE_TAURI_CONFIG: tauriConfig,
            SOURCE_COMMIT: "e".repeat(40),
          },
        },
      );

      const generated = JSON.parse(
        await readFile(resolve(bundleDirectory, "release-artifacts.json"), "utf8"),
      );
      expect(generated.sourceCommit).toBe("e".repeat(40));
      expect(await readFile(historicalPath, "utf8")).toBe(historicalBefore);
    } finally {
      await rm(fixtureRoot, { recursive: true, force: true });
    }
  });

  it("reports an unbuilt local artifact without leaking its bundle path", async () => {
    const secretMarker = "CUSTOMER-SHARE-SECRET";
    const fixtureRoot = await mkdtemp(
      resolve(tmpdir(), `photo-release-${secretMarker}-`),
    );
    const bundleDirectory = resolve(fixtureRoot, "bundle");
    const output = resolve(fixtureRoot, "release.json");

    try {
      await execFileAsync(
        process.execPath,
        [resolve(root, "scripts/generate-release-metadata.mjs"), output],
        {
          cwd: root,
          env: {
            ...process.env,
            RELEASE_BUNDLE_DIRECTORY: bundleDirectory,
            SOURCE_COMMIT: "a".repeat(40),
          },
        },
      );

      const contents = await readFile(output, "utf8");
      const metadata = JSON.parse(contents);

      expect(metadata.version).toBe("0.1.24");
      expect(metadata.macos.status).toBe("not-built-on-this-host");
      expect(
        metadata.windows.expectedArtifacts.map(
          ({ filename }: { filename: string }) => filename,
        ),
      ).toEqual([
        "照片筛选助手_0.1.24_x64-setup.exe",
        "照片筛选助手_0.1.24_x64_zh-CN.msi",
      ]);
      expect(contents).not.toContain(secretMarker);
      expect(contents).not.toContain(bundleDirectory);
    } finally {
      await rm(fixtureRoot, { recursive: true, force: true });
    }
  });

  it("uses the WiX language from the Tauri config as the MSI filename source", async () => {
    const fixtureRoot = await mkdtemp(resolve(tmpdir(), "photo-release-locale-"));
    const bundleDirectory = resolve(fixtureRoot, "bundle");
    const output = resolve(fixtureRoot, "release.json");
    const tauriConfig = resolve(fixtureRoot, "tauri.conf.json");

    try {
      await writeFile(
        tauriConfig,
        JSON.stringify({ bundle: { windows: { wix: { language: "ja-JP" } } } }),
      );
      await execFileAsync(
        process.execPath,
        [resolve(root, "scripts/generate-release-metadata.mjs"), output],
        {
          cwd: root,
          env: {
            ...process.env,
            RELEASE_BUNDLE_DIRECTORY: bundleDirectory,
            RELEASE_TAURI_CONFIG: tauriConfig,
            SOURCE_COMMIT: "c".repeat(40),
          },
        },
      );

      const metadata = JSON.parse(await readFile(output, "utf8"));
      expect(metadata.windows.expectedArtifacts[1]).toMatchObject({
        kind: "msi",
        filename: "照片筛选助手_0.1.24_x64_ja-JP.msi",
        relativePath:
          "src-tauri/target/release/bundle/msi/照片筛选助手_0.1.24_x64_ja-JP.msi",
      });
    } finally {
      await rm(fixtureRoot, { recursive: true, force: true });
    }
  });

  it.each([
    [{ bundle: { windows: { wix: {} } } }, "bundle.windows.wix.language"],
    [
      { bundle: { windows: { wix: { language: "zh_cn/unsafe" } } } },
      "valid WiX locale",
    ],
  ])("rejects missing or invalid WiX language config", async (config, message) => {
    const fixtureRoot = await mkdtemp(resolve(tmpdir(), "photo-release-bad-locale-"));
    const bundleDirectory = resolve(fixtureRoot, "bundle");
    const output = resolve(fixtureRoot, "release.json");
    const tauriConfig = resolve(fixtureRoot, "tauri.conf.json");

    try {
      await writeFile(tauriConfig, JSON.stringify(config));
      await expect(
        execFileAsync(
          process.execPath,
          [resolve(root, "scripts/generate-release-metadata.mjs"), output],
          {
            cwd: root,
            env: {
              ...process.env,
              RELEASE_BUNDLE_DIRECTORY: bundleDirectory,
              RELEASE_TAURI_CONFIG: tauriConfig,
              SOURCE_COMMIT: "d".repeat(40),
            },
          },
        ),
      ).rejects.toMatchObject({
        stderr: expect.stringContaining(message),
      });
    } finally {
      await rm(fixtureRoot, { recursive: true, force: true });
    }
  });

  it("rejects a DMG built for a different application version", async () => {
    const fixtureRoot = await mkdtemp(resolve(tmpdir(), "photo-release-stale-"));
    const bundleDirectory = resolve(fixtureRoot, "bundle");
    const output = resolve(fixtureRoot, "release.json");
    const dmgDirectory = resolve(bundleDirectory, "dmg");

    try {
      await mkdir(dmgDirectory, { recursive: true });
      await writeFile(
        resolve(dmgDirectory, "照片筛选助手_0.1.0_aarch64.dmg"),
        "stale artifact",
      );

      await expect(
        execFileAsync(
          process.execPath,
          [resolve(root, "scripts/generate-release-metadata.mjs"), output],
          {
            cwd: root,
            env: {
              ...process.env,
              RELEASE_BUNDLE_DIRECTORY: bundleDirectory,
              SOURCE_COMMIT: "b".repeat(40),
            },
          },
        ),
      ).rejects.toMatchObject({
        stderr: expect.stringContaining("0.1.24"),
      });
    } finally {
      await rm(fixtureRoot, { recursive: true, force: true });
    }
  });

  it("accepts a DMG only when its adjacent provenance matches bytes and source commit", async () => {
    const fixtureRoot = await mkdtemp(resolve(tmpdir(), "photo-release-valid-provenance-"));
    const bundleDirectory = resolve(fixtureRoot, "bundle");
    const output = resolve(fixtureRoot, "release.json");
    const sourceCommit = "f".repeat(40);

    try {
      await writeDmgFixture(bundleDirectory, sourceCommit);
      await execFileAsync(
        process.execPath,
        [resolve(root, "scripts/generate-release-metadata.mjs"), output],
        {
          cwd: root,
          env: { ...process.env, RELEASE_BUNDLE_DIRECTORY: bundleDirectory, SOURCE_COMMIT: sourceCommit },
        },
      );

      const metadata = JSON.parse(await readFile(output, "utf8"));
      expect(metadata.macos).toMatchObject({
        status: "built",
        artifact: {
          filename: "照片筛选助手_0.1.24_aarch64.dmg",
          sourceCommit,
        },
      });
    } finally {
      await rm(fixtureRoot, { recursive: true, force: true });
    }
  });

  it("rejects a current-version DMG that has no provenance sidecar", async () => {
    const fixtureRoot = await mkdtemp(resolve(tmpdir(), "photo-release-missing-provenance-"));
    const bundleDirectory = resolve(fixtureRoot, "bundle");
    const output = resolve(fixtureRoot, "release.json");
    const dmgDirectory = resolve(bundleDirectory, "dmg");

    try {
      await mkdir(dmgDirectory, { recursive: true });
      await writeFile(
        resolve(dmgDirectory, "照片筛选助手_0.1.24_aarch64.dmg"),
        "old same-version artifact",
      );
      await expect(
        execFileAsync(
          process.execPath,
          [resolve(root, "scripts/generate-release-metadata.mjs"), output],
          {
            cwd: root,
            env: {
              ...process.env,
              RELEASE_BUNDLE_DIRECTORY: bundleDirectory,
              SOURCE_COMMIT: "1".repeat(40),
            },
          },
        ),
      ).rejects.toMatchObject({
        stderr: expect.stringContaining("provenance sidecar"),
      });
    } finally {
      await rm(fixtureRoot, { recursive: true, force: true });
    }
  });

  it.each([
    ["stale source", { sourceCommit: "2".repeat(40) }, "sourceCommit"],
    ["hash mismatch", { sha256: "0".repeat(64) }, "sha256"],
    ["size mismatch", { sizeBytes: 99 }, "sizeBytes"],
    ["filename mismatch", { filename: "旧产物.dmg" }, "filename"],
  ])("rejects %s provenance", async (_label, overrides, expectedMessage) => {
    const fixtureRoot = await mkdtemp(resolve(tmpdir(), "photo-release-bad-provenance-"));
    const bundleDirectory = resolve(fixtureRoot, "bundle");
    const output = resolve(fixtureRoot, "release.json");
    const sourceCommit = "3".repeat(40);

    try {
      await writeDmgFixture(bundleDirectory, sourceCommit, overrides);
      await expect(
        execFileAsync(
          process.execPath,
          [resolve(root, "scripts/generate-release-metadata.mjs"), output],
          {
            cwd: root,
            env: { ...process.env, RELEASE_BUNDLE_DIRECTORY: bundleDirectory, SOURCE_COMMIT: sourceCommit },
          },
        ),
      ).rejects.toMatchObject({
        stderr: expect.stringContaining(expectedMessage),
      });
    } finally {
      await rm(fixtureRoot, { recursive: true, force: true });
    }
  });

  it("routes each platform package through the single provenance-aware wrapper", async () => {
    const packageJson = JSON.parse(
      await readFile(resolve(root, "package.json"), "utf8"),
    );
    expect(packageJson.scripts["package:macos"]).toBe(
      "pnpm prepare:distribution-notices && node scripts/package-release.mjs macos",
    );
    expect(packageJson.scripts["package:windows"]).toBe(
      "pnpm prepare:distribution-notices && node scripts/package-release.mjs windows",
    );
    expect(packageJson.scripts["package:macos"]).not.toContain(
      "write-release-provenance",
    );
  });
});

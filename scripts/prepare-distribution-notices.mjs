import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  lstat,
  mkdir,
  readdir,
  readFile,
  realpath,
  rm,
  writeFile,
} from "node:fs/promises";
import { basename, dirname, extname, relative, resolve, sep } from "node:path";

const root = resolve(import.meta.dirname, "..");
const outputDirectory = resolve(root, "public/third-party-licenses");
const outputTextDirectory = resolve(outputDirectory, "texts");
const licenseNamePattern = /(?:^|[._-])(LICENSE|COPYING|NOTICE|COPYRIGHT|UNLICENSE)(?:[._-]|$)/iu;
const ignoredNodeModulesDirectories = new Set([".bin", ".vite", ".vite-temp"]);
const ignoredNoticeDirectories = new Set([".git", "node_modules", "public", "target", "dist"]);

function packageKey(ecosystem, name, version) {
  return `${ecosystem}:${name}@${version}`;
}

function safePart(value) {
  return String(value || "unknown")
    .replace(/[^a-z0-9._-]+/giu, "_")
    .replace(/^\.+/u, "_")
    .slice(0, 160) || "unknown";
}

function safeRelativePath(value) {
  const parts = String(value).split(/[\\/]+/u).filter(Boolean);
  return parts
    .filter(part => part !== "." && part !== "..")
    .map(safePart)
    .join("/");
}

function asHttpUrl(value) {
  if (typeof value !== "string") return null;
  const trimmed = value.trim();
  if (!trimmed) return null;
  const normalized = trimmed
    .replace(/^git\+https:\/\//iu, "https://")
    .replace(/^git\+http:\/\//iu, "http://");
  if (!/^https?:\/\//iu.test(normalized)) return null;
  try {
    const parsed = new URL(normalized);
    if (parsed.username || parsed.password) return null;
    return parsed.toString();
  } catch {
    return null;
  }
}

function repositoryUrl(repository) {
  if (typeof repository === "string") return asHttpUrl(repository);
  if (repository && typeof repository === "object") {
    return asHttpUrl(repository.url) || asHttpUrl(repository.directory);
  }
  return null;
}

function npmSourceUrl(name, version) {
  const encodedName = String(name)
    .split("/")
    .map(part => encodeURIComponent(part))
    .join("/");
  return `https://www.npmjs.com/package/${encodedName}/v/${encodeURIComponent(version)}`;
}

function cargoSourceUrl(pkg) {
  const repository = repositoryUrl(pkg.repository);
  if (repository) return repository;
  if (typeof pkg.source === "string") {
    const source = pkg.source.replace(/^[a-z]+\+/iu, "");
    if (asHttpUrl(source)) return `https://crates.io/crates/${encodeURIComponent(pkg.name)}/${encodeURIComponent(pkg.version)}`;
  }
  return `https://crates.io/crates/${encodeURIComponent(pkg.name)}/${encodeURIComponent(pkg.version)}`;
}

function normalizeAuthors(value) {
  if (Array.isArray(value)) return value.filter(item => typeof item === "string");
  if (typeof value === "string" && value.trim()) return [value.trim()];
  if (value && typeof value === "object" && typeof value.name === "string") {
    return [value.name.trim()].filter(Boolean);
  }
  return [];
}

function normalizeLicense(value) {
  if (typeof value !== "string" || !value.trim()) return null;
  return value.trim();
}

async function readJson(path) {
  try {
    return JSON.parse(await readFile(path, "utf8"));
  } catch (error) {
    if (error?.code === "ENOENT" || error instanceof SyntaxError) return null;
    throw error;
  }
}

async function packageManifest(path) {
  const manifestPath = resolve(path, "package.json");
  const manifest = await readJson(manifestPath);
  if (!manifest || typeof manifest.name !== "string" || typeof manifest.version !== "string") {
    return null;
  }
  return { manifest, manifestPath };
}

async function collectNodePackageManifests(nodeModulesDirectory) {
  const packages = new Map();
  const visited = new Set();

  async function visit(directory) {
    let canonical;
    try {
      canonical = await realpath(directory);
    } catch (error) {
      if (error?.code === "ENOENT") return;
      throw error;
    }
    if (visited.has(canonical)) return;
    visited.add(canonical);
    const isPnpmStore = basename(directory) === ".pnpm";

    let entries;
    try {
      entries = await readdir(directory, { withFileTypes: true });
    } catch (error) {
      if (error?.code === "ENOENT") return;
      throw error;
    }
    for (const entry of entries) {
      if (!entry.isDirectory() && !entry.isSymbolicLink()) continue;
      if (ignoredNodeModulesDirectories.has(entry.name)) continue;
      const child = resolve(directory, entry.name);

      if (entry.name === ".pnpm") {
        await visit(child);
        continue;
      }
      if (isPnpmStore) {
        await visit(child);
        continue;
      }
      if (entry.name === "node_modules") {
        await visit(child);
        continue;
      }

      const manifest = await packageManifest(child);
      if (manifest) {
        const key = packageKey("node", manifest.manifest.name, manifest.manifest.version);
        const existing = packages.get(key);
        if (existing) {
          existing.roots.push(dirname(manifest.manifestPath));
        } else {
          packages.set(key, {
            ecosystem: "node",
            name: manifest.manifest.name,
            version: manifest.manifest.version,
            license: normalizeLicense(manifest.manifest.license),
            sourceUrl: repositoryUrl(manifest.manifest.repository)
              || asHttpUrl(manifest.manifest.homepage)
              || npmSourceUrl(manifest.manifest.name, manifest.manifest.version),
            sourceDownloadUrl: npmSourceUrl(manifest.manifest.name, manifest.manifest.version),
            authors: normalizeAuthors(manifest.manifest.author),
            licenseFileHint: null,
            roots: [dirname(manifest.manifestPath)],
          });
        }
        const nestedNodeModules = resolve(child, "node_modules");
        await visit(nestedNodeModules).catch(error => {
          if (error?.code !== "ENOENT") throw error;
        });
        continue;
      }
      if (entry.name.startsWith("@")) {
        await visit(child);
      }
    }
  }

  await visit(nodeModulesDirectory);
  return [...packages.values()].sort((a, b) =>
    `${a.name}@${a.version}`.localeCompare(`${b.name}@${b.version}`),
  );
}

function cargoManifestPath() {
  const nested = resolve(root, "src-tauri/Cargo.toml");
  return nested;
}

function cargoMetadata() {
  const output = execFileSync(
    "cargo",
    ["metadata", "--format-version", "1", "--locked", "--manifest-path", cargoManifestPath()],
    { cwd: root, encoding: "utf8", maxBuffer: 64 * 1024 * 1024 },
  );
  const metadata = JSON.parse(output);
  if (!Array.isArray(metadata.packages)) {
    throw new Error("cargo metadata did not return a packages array.");
  }
  return metadata.packages;
}

async function textCandidates(directory, hint) {
  const candidates = new Set();
  if (typeof hint === "string" && hint.trim()) {
    const hinted = resolve(directory, hint);
    if (hinted === directory || hinted.startsWith(`${directory}${sep}`)) candidates.add(hinted);
  }
  const visited = new Set();
  async function visit(current) {
    let details;
    try {
      details = await lstat(current);
    } catch (error) {
      if (error?.code === "ENOENT") return;
      throw error;
    }
    if (details.isSymbolicLink()) {
      try {
        details = await lstat(await realpath(current));
      } catch (error) {
        if (error?.code === "ENOENT") return;
        throw error;
      }
    }
    if (!details.isDirectory()) {
      if (licenseNamePattern.test(basename(current))) candidates.add(current);
      return;
    }
    const canonical = details.dev !== undefined && details.ino !== undefined
      ? `${details.dev}:${details.ino}`
      : current;
    if (visited.has(canonical)) return;
    visited.add(canonical);
    let entries;
    try {
      entries = await readdir(current, { withFileTypes: true });
    } catch (error) {
      if (error?.code === "ENOENT") return;
      throw error;
    }
    for (const entry of entries) {
      if (ignoredNoticeDirectories.has(entry.name)) continue;
      await visit(resolve(current, entry.name));
    }
  }
  await visit(directory);
  return [...candidates].sort();
}

async function copyNoticeFile(source, destinationDirectory, relativeName, copied) {
  try {
    const details = await lstat(source);
    if (!details.isFile()) return null;
  } catch (error) {
    if (error?.code === "ENOENT") return null;
    throw error;
  }
  let content;
  try {
    content = await readFile(source);
  } catch {
    return null;
  }
  if (content.includes(0)) return null;
  if (content.byteLength > 10 * 1024 * 1024) return null;
  const safeName = safeRelativePath(relativeName) || "NOTICE";
  const hash = createHash("sha256").update(content).digest("hex");
  let destination = resolve(destinationDirectory, safeName);
  if (!destination.startsWith(`${destinationDirectory}${sep}`)) return null;
  const destinationRelative = relative(outputDirectory, destination).split(sep).join("/");
  if (destinationRelative.split("/").includes("..")) return null;
  const existing = copied.get(destinationRelative);
  if (existing === hash) return destinationRelative;
  if (existing && existing !== hash) {
    const extension = extname(destination);
    const stem = extension ? destination.slice(0, -extension.length) : destination;
    destination = `${stem}-${hash.slice(0, 8)}${extension}`;
  }
  await mkdir(dirname(destination), { recursive: true });
  await writeFile(destination, content);
  const finalRelative = relative(outputDirectory, destination).split(sep).join("/");
  copied.set(finalRelative, hash);
  return finalRelative;
}

async function collectPackageNotices(pkg, copied) {
  const destinationDirectory = resolve(
    outputTextDirectory,
    `${safePart(pkg.ecosystem)}-${safePart(pkg.name)}-${safePart(pkg.version)}`,
  );
  const files = new Set();
  for (const packageRoot of pkg.roots) {
    const candidates = await textCandidates(packageRoot, pkg.licenseFileHint);
    for (const source of candidates) {
      const relativeName = relative(packageRoot, source);
      const target = await copyNoticeFile(source, destinationDirectory, relativeName, copied);
      if (target) files.add(target);
    }
  }
  return [...files].sort();
}

function rustPackageRecord(pkg) {
  return {
    ecosystem: "rust",
    name: pkg.name,
    version: pkg.version,
    license: normalizeLicense(pkg.license),
    sourceUrl: cargoSourceUrl(pkg),
    sourceDownloadUrl: `https://crates.io/api/v1/crates/${encodeURIComponent(pkg.name)}/${encodeURIComponent(pkg.version)}/download`,
    authors: normalizeAuthors(pkg.authors),
    licenseFileHint: typeof pkg.license_file === "string" ? pkg.license_file : null,
    roots: [dirname(pkg.manifest_path)],
  };
}

function packageSort(a, b) {
  return `${a.ecosystem}:${a.name}@${a.version}`.localeCompare(
    `${b.ecosystem}:${b.name}@${b.version}`,
  );
}

async function copyApplicationFiles(copied) {
  const applicationDirectory = resolve(outputTextDirectory, "application");
  const files = [];
  for (const filename of ["LICENSE", "THIRD_PARTY_NOTICES.md"]) {
    const source = resolve(root, filename);
    const target = await copyNoticeFile(source, applicationDirectory, filename, copied);
    if (target) files.push(target);
  }
  return files;
}

function noticeHeader(record) {
  return [
    `## ${record.ecosystem}: ${record.name} ${record.version}`,
    `License: ${record.license || "not declared"}`,
    `Source: ${record.sourceUrl}`,
    record.authors.length ? `Authors: ${record.authors.join(", ")}` : null,
    `License text files: ${record.licenseFiles.length ? record.licenseFiles.join(", ") : "not found in the installed package"}`,
    "",
  ].filter(Boolean).join("\n");
}

async function main() {
  const cargoManifest = resolve(cargoManifestPath());
  const rustPackages = cargoMetadata()
    .filter(pkg => resolve(pkg.manifest_path) !== cargoManifest)
    .map(rustPackageRecord);
  const nodeModulesDirectory = resolve(root, "node_modules");
  const nodePackages = await collectNodePackageManifests(nodeModulesDirectory);
  const packages = [...rustPackages, ...nodePackages].sort(packageSort);

  await rm(outputDirectory, { recursive: true, force: true });
  await mkdir(outputTextDirectory, { recursive: true });
  const copied = new Map();
  const applicationFiles = await copyApplicationFiles(copied);
  const inventoryPackages = [];
  for (const pkg of packages) {
    const licenseFiles = await collectPackageNotices(pkg, copied);
    inventoryPackages.push({
      ecosystem: pkg.ecosystem,
      name: pkg.name,
      version: pkg.version,
      license: pkg.license,
      sourceUrl: pkg.sourceUrl,
      sourceDownloadUrl: pkg.sourceDownloadUrl,
      authors: pkg.authors,
      licenseFiles,
      licenseTextFound: licenseFiles.length > 0,
      licenseTextStatus: licenseFiles.length > 0 ? "found" : "not-found",
    });
  }

  const missingLicenseText = inventoryPackages
    .filter(pkg => !pkg.licenseTextFound)
    .map(pkg => `${pkg.ecosystem}:${pkg.name}@${pkg.version}`);
  const inventory = {
    schemaVersion: 1,
    application: {
      name: "photo-selection-assistant",
      version: (await readJson(resolve(root, "package.json")))?.version || null,
      license: "MIT",
      noticeFiles: applicationFiles,
    },
    packages: inventoryPackages,
    summary: {
      packageCount: inventoryPackages.length,
      licenseFileCount: copied.size,
      packagesWithoutLicenseText: missingLicenseText,
    },
  };
  await writeFile(
    resolve(outputDirectory, "inventory.json"),
    `${JSON.stringify(inventory, null, 2)}\n`,
    "utf8",
  );

  const readme = [
    "# Distribution third-party notices",
    "",
    "This generated directory accompanies the macOS and Windows application packages.",
    "It contains sanitized dependency metadata and the license, notice, and copyright text found in the installed dependency trees.",
    "",
    `Packages collected: ${inventory.summary.packageCount}`,
    `License and notice files copied: ${inventory.summary.licenseFileCount}`,
    `Packages without a license text file: ${missingLicenseText.length}`,
    missingLicenseText.length ? `Missing package texts: ${missingLicenseText.join(", ")}` : "Missing package texts: none",
    "",
    "`inventory.json` records the declared license and source URL for every collected package. A `not-found` license text status means no matching upstream text file was present in the installed package; it is not an assertion that the package is unlicensed.",
    "",
  ].join("\n");
  await writeFile(resolve(outputDirectory, "README.md"), readme, "utf8");

  const aggregate = [
    "THIRD-PARTY NOTICES",
    "",
    "The following metadata and upstream text files are included with the distribution.",
    "",
    ...inventoryPackages.flatMap(record => {
      const headings = [noticeHeader(record)];
      return headings;
    }),
  ];
  for (const record of inventoryPackages) {
    for (const licenseFile of record.licenseFiles) {
      const source = resolve(outputDirectory, licenseFile);
      const content = await readFile(source, "utf8");
      aggregate.push(`### ${licenseFile}`, "", content.trimEnd(), "");
    }
  }
  await writeFile(resolve(outputDirectory, "THIRD_PARTY_NOTICES.txt"), `${aggregate.join("\n")}\n`, "utf8");

  console.log(
    `Prepared distribution notices: ${rustPackages.length} Rust packages, ${nodePackages.length} Node packages, ${copied.size} license/notice files.`,
  );
  if (missingLicenseText.length) {
    console.log(`License text not found for ${missingLicenseText.length} packages: ${missingLicenseText.join(", ")}`);
  } else {
    console.log("License text not found for 0 packages.");
  }
}

await main();

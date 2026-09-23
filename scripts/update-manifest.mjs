#!/usr/bin/env node
// 把各平台的打包碎片合并进 wand-render-bin 的 manifest.json，并落盘二进制。
//
// 用法：node scripts/update-manifest.js --bin-dir ../wand-render-bin [--dry-run]
//
// 为什么需要它：四个平台各自在自己的 runner 上构建，谁都不该去改共享的 manifest；
// 所以由最后一个 job 汇总碎片，一次性重写 manifest.json 与 version 目录。
import { copyFileSync, mkdirSync, chmodSync, existsSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import process from "node:process";

function parseArgs(argv) {
  const options = { binDir: null, distDir: "dist", dryRun: false };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === "--bin-dir") options.binDir = argv[++i];
    else if (arg === "--dist-dir") options.distDir = argv[++i];
    else if (arg === "--dry-run") options.dryRun = true;
    else throw new Error(`unknown argument: ${arg}`);
  }
  if (!options.binDir) throw new Error("--bin-dir is required");
  return options;
}

function readJson(file) {
  return JSON.parse(readFileSync(file, "utf8"));
}

function main() {
  const options = parseArgs(process.argv.slice(2));
  const distDir = path.resolve(options.distDir);
  const fragmentsDir = path.join(distDir, "fragments");
  if (!existsSync(fragmentsDir)) throw new Error(`no fragments at ${fragmentsDir}; run scripts/package-release.sh first`);

  const fragments = readdirSync(fragmentsDir)
    .filter((name) => name.endsWith(".json"))
    .map((name) => readJson(path.join(fragmentsDir, name)));
  if (fragments.length === 0) throw new Error("no fragments found");

  const versions = new Set(fragments.map((fragment) => fragment.version));
  if (versions.size !== 1) throw new Error(`mixed versions in fragments: ${[...versions].join(", ")}`);
  const version = [...versions][0];

  const manifestPath = path.join(options.binDir, "manifest.json");
  const manifest = existsSync(manifestPath)
    ? readJson(manifestPath)
    : { schemaVersion: 1, latest: version, versions: {} };
  if (manifest.schemaVersion !== 1) throw new Error(`unsupported manifest schemaVersion ${manifest.schemaVersion}`);

  const entry = manifest.versions[version] ?? {
    protocolVersion: fragments[0].protocolVersion,
    minServerVersion: fragments[0].minServerVersion,
    triples: {},
  };
  // 同一版本内不允许协议版本漂移：协议变了就必须是新版本号。
  if (entry.protocolVersion !== fragments[0].protocolVersion) {
    throw new Error(`protocol mismatch for ${version}: manifest=${entry.protocolVersion} fragment=${fragments[0].protocolVersion}`);
  }

  for (const fragment of fragments) {
    const sourceDir = path.join(distDir, `v${version}`, fragment.triple);
    const targetDir = path.join(options.binDir, `v${version}`, fragment.triple);
    if (!options.dryRun) {
      mkdirSync(targetDir, { recursive: true });
      for (const name of ["wand-render", "wand-render.version", "wand-render.sha256"]) {
        copyFileSync(path.join(sourceDir, name), path.join(targetDir, name));
      }
      chmodSync(path.join(targetDir, "wand-render"), 0o755);
    }
    entry.triples[fragment.triple] = {
      path: `v${version}/${fragment.triple}/wand-render`,
      sha256: fragment.sha256,
      size: fragment.size,
      rustTarget: fragment.rustTarget,
    };
  }
  manifest.versions[version] = entry;
  manifest.latest = version;

  if (options.dryRun) {
    process.stdout.write(`[manifest] dry-run: would write ${version} with ${Object.keys(entry.triples).join(", ")}\n`);
    return;
  }
  writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`, "utf8");
  process.stdout.write(`[manifest] ${manifestPath}: ${version} -> ${Object.keys(entry.triples).join(", ")}\n`);
}

main();

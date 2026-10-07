// Downloads the OxideClaw release binary for this platform and verifies its
// SHA-256 against the sidecar the release workflow publishes. No dependencies.
"use strict";
const fs = require("fs");
const os = require("os");
const path = require("path");
const https = require("https");
const crypto = require("crypto");

const pkg = require("./package.json");
const VERSION = pkg.version;
const REPO = "ForkedInTime/OxideClaw";

function assetName() {
  const arch = { x64: "x64", arm64: "arm64" }[process.arch];
  if (!arch) throw new Error(`unsupported CPU ${process.arch}`);
  switch (process.platform) {
    case "linux": {
      // The gnu builds need glibc 2.28+ and will not exec on musl (Alpine),
      // where the glibc loader is absent. Mirror install.sh: x64 falls back
      // to the static musl build; arm64 has none.
      const glibc = glibcVersion();
      const tooOld = glibc && compareVersions(glibc, "2.28") < 0;
      if (!glibc || tooOld) {
        if (arch === "x64") return "oxideclaw-linux-x64-musl";
        throw new Error(
          (glibc ? `glibc ${glibc} is older than 2.28, which the prebuilt arm64 binary needs.`
                 : "no prebuilt musl binary for arm64.")
          + " Build from source: cargo install oxideclaw");
      }
      return `oxideclaw-linux-${arch}`;
    }
    case "darwin": return `oxideclaw-macos-${arch}`;
    case "win32":
      if (arch !== "x64") throw new Error("Windows arm64 builds are not published yet");
      return "oxideclaw-windows-x64.exe";
    default: throw new Error(`unsupported OS ${process.platform}`);
  }
}

// Node's diagnostic report carries the runtime glibc version; it is absent
// when Node itself runs on musl.
function glibcVersion() {
  try {
    process.report.excludeNetwork = true;
    return process.report.getReport().header.glibcVersionRuntime || null;
  } catch {
    return null;
  }
}

function compareVersions(a, b) {
  const pa = a.split(".").map(Number);
  const pb = b.split(".").map(Number);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
    const d = (pa[i] || 0) - (pb[i] || 0);
    if (d) return d;
  }
  return 0;
}

function get(url, redirects = 0) {
  return new Promise((resolve, reject) => {
    https.get(url, { headers: { "User-Agent": `oxideclaw-npm/${VERSION}` } }, (res) => {
      if ([301, 302, 303, 307, 308].includes(res.statusCode) && res.headers.location && redirects < 5) {
        res.resume();
        return resolve(get(res.headers.location, redirects + 1));
      }
      if (res.statusCode !== 200) {
        res.resume();
        return reject(new Error(`${url}: HTTP ${res.statusCode}`));
      }
      const chunks = [];
      res.on("data", (c) => chunks.push(c));
      res.on("end", () => resolve(Buffer.concat(chunks)));
      res.on("error", reject);
    }).on("error", reject);
  });
}

async function main() {
  const asset = assetName();
  const base = `https://github.com/${REPO}/releases/download/v${VERSION}/${asset}`;
  const [bin, sums] = await Promise.all([get(base), get(`${base}.sha256`)]);
  const expected = sums.toString("utf8").trim().split(/\s+/)[0].toLowerCase();
  const actual = crypto.createHash("sha256").update(bin).digest("hex");
  if (actual !== expected) throw new Error(`checksum mismatch for ${asset}: ${actual} != ${expected}`);
  const dir = path.join(__dirname, "vendor");
  fs.mkdirSync(dir, { recursive: true });
  const out = path.join(dir, process.platform === "win32" ? "oxideclaw.exe" : "oxideclaw");
  fs.writeFileSync(out, bin, { mode: 0o755 });
  console.log(`oxideclaw ${VERSION}: installed ${asset} (${(bin.length / 1048576).toFixed(1)} MB, sha256 verified)`);
}

if (require.main === module) {
  main().catch((e) => {
    console.error(`oxideclaw: install failed: ${e.message}`);
    console.error(`You can install the binary directly: https://github.com/${REPO}/releases/tag/v${VERSION}`);
    process.exit(1);
  });
}

module.exports = { assetName };

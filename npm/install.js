// Downloads the OxideClaw release binary for this platform and verifies its
// SHA-256 against the sidecar the release workflow publishes. No dependencies.
"use strict";
const fs = require("fs");
const os = require("os");
const path = require("path");
const http = require("http");
const https = require("https");
const tls = require("tls");
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

// Idle limit on every socket: a stalled connection or transfer errors out
// instead of hanging `npm install` forever.
const TIMEOUT_MS = 30000;

// Node's https module ignores proxy settings, so on proxy-only networks the
// postinstall would try (and fail) to reach GitHub directly even though npm
// itself fetched the package through the proxy. Honor npm's config first,
// then the usual environment variables.
function proxyFor(host) {
  const env = process.env;
  const noProxy = env.npm_config_noproxy || env.NO_PROXY || env.no_proxy || "";
  for (let entry of noProxy.split(/[\s,]+/)) {
    entry = entry.replace(/:\d+$/, "").replace(/^\*?\./, "").toLowerCase();
    if (!entry) continue;
    if (entry === "*" || host === entry || host.endsWith(`.${entry}`)) return null;
  }
  const raw = [env.npm_config_https_proxy, env.npm_config_proxy, env.HTTPS_PROXY, env.https_proxy]
    .find((v) => v && v !== "null" && v !== "false");
  if (!raw) return null;
  return new URL(raw.includes("://") ? raw : `http://${raw}`);
}

function tunnel(proxy, host, port) {
  return new Promise((resolve, reject) => {
    const secure = proxy.protocol === "https:";
    const headers = { Host: `${host}:${port}` };
    if (proxy.username) {
      const creds = `${decodeURIComponent(proxy.username)}:${decodeURIComponent(proxy.password)}`;
      headers["Proxy-Authorization"] = `Basic ${Buffer.from(creds).toString("base64")}`;
    }
    const req = (secure ? https : http).request({
      host: proxy.hostname,
      port: proxy.port || (secure ? 443 : 80),
      method: "CONNECT",
      path: `${host}:${port}`,
      headers,
      agent: false,
    });
    req.setTimeout(TIMEOUT_MS, () => req.destroy(new Error(`proxy ${proxy.host}: timed out`)));
    req.on("connect", (res, socket) => {
      socket.setTimeout(0);
      if (res.statusCode !== 200) {
        socket.destroy();
        return reject(new Error(`proxy ${proxy.host}: CONNECT ${host} returned HTTP ${res.statusCode}`));
      }
      resolve(socket);
    });
    req.on("error", reject);
    req.end();
  });
}

async function get(url, redirects = 0) {
  const { hostname, port } = new URL(url);
  const proxy = proxyFor(hostname.toLowerCase());
  const opts = { headers: { "User-Agent": `oxideclaw-npm/${VERSION}` } };
  if (proxy) {
    const socket = await tunnel(proxy, hostname, port || 443);
    // No `agent`: with `agent: false` Node builds a fresh Agent, which dials
    // the host itself and never calls this, so the tunnel went unused.
    opts.createConnection = () => tls.connect({ socket, servername: hostname });
  }
  return new Promise((resolve, reject) => {
    const req = https.get(url, opts, (res) => {
      if ([301, 302, 303, 307, 308].includes(res.statusCode) && res.headers.location && redirects < 5) {
        res.resume();
        return resolve(get(new URL(res.headers.location, url).href, redirects + 1));
      }
      if (res.statusCode !== 200) {
        res.resume();
        return reject(new Error(`${url}: HTTP ${res.statusCode}`));
      }
      const chunks = [];
      res.on("data", (c) => chunks.push(c));
      res.on("end", () => resolve(Buffer.concat(chunks)));
      res.on("error", reject);
    });
    req.setTimeout(TIMEOUT_MS, () => req.destroy(new Error(`${url}: timed out`)));
    req.on("error", reject);
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

module.exports = { assetName, get, proxyFor };

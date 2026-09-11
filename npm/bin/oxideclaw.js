#!/usr/bin/env node
// Thin launcher: runs the vendored native binary with the same argv and TTY.
// If the postinstall step was skipped (npm's install-scripts policy, --ignore-scripts),
// the binary is fetched on first run instead.
"use strict";
const fs = require("fs");
const path = require("path");
const { spawnSync } = require("child_process");
const root = path.join(__dirname, "..");
const exe = path.join(root, "vendor", process.platform === "win32" ? "oxideclaw.exe" : "oxideclaw");
if (!fs.existsSync(exe)) {
  const r = spawnSync(process.execPath, [path.join(root, "install.js")], { stdio: "inherit" });
  if (r.status !== 0) process.exit(r.status === null ? 1 : r.status);
}
const r = spawnSync(exe, process.argv.slice(2), { stdio: "inherit" });
if (r.error) {
  console.error(`oxideclaw: cannot start native binary (${r.error.message})`);
  process.exit(1);
}
process.exit(r.status === null ? 1 : r.status);

#!/usr/bin/env node
// A stand-in for `vite build`: writes index.html to dist/ with the greeting that
// @fixture/greeting's own build produced, so it fails when that build did not run.
const fs = require("node:fs");
const path = require("node:path");

if (process.argv[2] !== "build") {
  console.error("this stand-in only builds");
  process.exit(1);
}
const greeting = fs.readFileSync(
  path.join(path.dirname(require.resolve("@fixture/greeting")), "dist", "greeting.txt"),
  "utf8",
);
fs.mkdirSync("dist", { recursive: true });
fs.writeFileSync(
  "dist/index.html",
  fs.readFileSync("index.html", "utf8").replace("__GREETING__", greeting),
);
console.log("built dist/index.html");

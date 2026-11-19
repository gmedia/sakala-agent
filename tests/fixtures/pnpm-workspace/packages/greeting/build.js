// Stands for the build step an internal package needs before an app can use it.
const fs = require("node:fs");
const { greeting } = require("./index.js");

fs.mkdirSync("dist", { recursive: true });
fs.writeFileSync("dist/greeting.txt", greeting);

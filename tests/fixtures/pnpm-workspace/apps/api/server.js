const http = require("node:http");
const { greeting } = require("@fixture/greeting");

http
  .createServer((request, response) => response.end(`api: ${greeting}\n`))
  .listen(process.env.PORT, "0.0.0.0", () => console.log("api listening on", process.env.PORT));

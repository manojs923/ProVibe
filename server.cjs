// Run: node server.cjs   (any Node version, no installs)  ->  http://localhost:8787
const http = require("http");
const https = require("https");
const fs = require("fs");
const path = require("path");
const PORT = process.env.PORT || 8787;

const CORS = {
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Methods": "GET,POST,PUT,PATCH,DELETE,OPTIONS",
  "Access-Control-Allow-Headers": "Authorization,Content-Type",
  "Access-Control-Allow-Private-Network": "true",
};

const server = http.createServer((req, res) => {
  if (req.method === "OPTIONS") { res.writeHead(204, CORS); return res.end(); }

  if (req.url === "/ping") {
    res.writeHead(200, { ...CORS, "Content-Type": "text/plain" });
    return res.end("ok");
  }

  if (req.url.startsWith("/v1/")) {
    const chunks = [];
    req.on("data", c => chunks.push(c));
    req.on("end", () => {
      const body = Buffer.concat(chunks);
      const up = https.request({
        hostname: "api.supabase.com",
        path: req.url,
        method: req.method,
        headers: {
          Authorization: req.headers["authorization"] || "",
          "Content-Type": req.headers["content-type"] || "application/json",
          "Content-Length": body.length,
          "User-Agent": "supacol-proxy",
        },
      }, r => {
        res.writeHead(r.statusCode, { ...CORS, "Content-Type": r.headers["content-type"] || "application/json" });
        r.pipe(res);
      });
      up.on("error", e => {
        res.writeHead(502, { ...CORS, "Content-Type": "application/json" });
        res.end(JSON.stringify({ message: "Proxy could not reach api.supabase.com: " + e.message }));
      });
      up.end(body);
    });
    return;
  }

  fs.readFile(path.join(__dirname, "supacol.html"), (err, data) => {
    if (err) { res.writeHead(404); return res.end("supacol.html must be in the same folder as server.cjs"); }
    res.writeHead(200, { "Content-Type": "text/html; charset=utf-8" });
    res.end(data);
  });
});

server.on("error", e => {
  if (e.code === "EADDRINUSE") console.log("Port " + PORT + " already in use — the proxy is probably already running. Open http://localhost:" + PORT);
  else console.log(e);
});
server.listen(PORT, () => console.log("supacol running -> http://localhost:" + PORT));
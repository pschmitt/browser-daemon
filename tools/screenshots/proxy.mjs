// A tiny forwarding proxy (HTTP and CONNECT) on 127.0.0.1:PORT, so the demo's
// "Office" and "VPN exit" proxies are real: the pages of the demo sessions load
// through them. Usage: node proxy.mjs PORT
import http from "node:http";
import net from "node:net";

const port = Number(process.argv[2]);

const server = http.createServer((req, res) => {
  const url = new URL(req.url);
  const upstream = http.request(
    { host: url.hostname, port: url.port || 80, path: url.pathname + url.search, method: req.method, headers: req.headers },
    (answer) => {
      res.writeHead(answer.statusCode, answer.headers);
      answer.pipe(res);
    },
  );
  upstream.on("error", () => {
    res.writeHead(502);
    res.end();
  });
  req.pipe(upstream);
});

server.on("connect", (req, socket, head) => {
  const [host, targetPort] = req.url.split(":");
  const upstream = net.connect(Number(targetPort) || 443, host, () => {
    socket.write("HTTP/1.1 200 Connection Established\r\n\r\n");
    upstream.write(head);
    upstream.pipe(socket);
    socket.pipe(upstream);
  });
  upstream.on("error", () => socket.destroy());
  socket.on("error", () => upstream.destroy());
});

server.listen(port, "127.0.0.1");

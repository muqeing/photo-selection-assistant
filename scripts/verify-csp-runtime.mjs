import { execFile, spawn } from "node:child_process";
import { once } from "node:events";
import { createServer } from "node:http";
import { mkdtemp, readFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { extname, resolve, sep } from "node:path";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);
const root = resolve(import.meta.dirname, "..");
const config = JSON.parse(
  await readFile(resolve(root, "src-tauri/tauri.conf.json"), "utf8"),
);
const csp = config?.app?.security?.csp;
if (typeof csp !== "string") {
  throw new Error("Tauri CSP is not configured.");
}

const chromeCandidates =
  process.platform === "darwin"
    ? [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
      ]
    : process.platform === "win32"
      ? [
          `${process.env.PROGRAMFILES ?? ""}\\Google\\Chrome\\Application\\chrome.exe`,
          `${process.env["PROGRAMFILES(X86)"] ?? ""}\\Google\\Chrome\\Application\\chrome.exe`,
        ]
      : ["google-chrome", "chromium", "chromium-browser"];

let chrome;
for (const candidate of chromeCandidates) {
  try {
    await execFileAsync(candidate, ["--version"]);
    chrome = candidate;
    break;
  } catch {
    // Try the next well-known executable.
  }
}
if (!chrome) {
  throw new Error("Chrome/Chromium is required for the CSP runtime smoke test.");
}

const distOcr = resolve(root, "dist/ocr");
const publicOcr = resolve(root, "public/ocr");
const ocrRoot = await stat(distOcr)
  .then(details => details.isDirectory() ? distOcr : publicOcr)
  .catch(() => publicOcr);
const tesseractBrowser = resolve(
  root,
  "node_modules/tesseract.js/dist/tesseract.min.js",
);

const page = `<!doctype html>
<html><head><meta charset="utf-8"><title>PENDING</title></head>
<body><div id="result">PENDING</div><script src="/vendor/tesseract.min.js"></script><script src="/smoke.js"></script></body></html>`;
const script = `
const failures = [];
window.addEventListener("securitypolicyviolation", (event) => {
  failures.push("CSP:" + event.violatedDirective + ":" + event.blockedURI);
});
window.addEventListener("error", (event) => {
  failures.push("ERROR:" + (event.message || "unknown"));
});
window.addEventListener("unhandledrejection", (event) => {
  failures.push("REJECTION:" + String(event.reason));
});

async function blobImageRoundTrip() {
  const image = new Image();
  const url = URL.createObjectURL(new Blob([
    '<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"><rect width="2" height="2" fill="white"/></svg>'
  ], { type: "image/svg+xml" }));
  try {
    await new Promise((resolve, reject) => {
      image.onload = resolve;
      image.onerror = () => reject(new Error("blob image failed"));
      image.src = url;
      document.body.append(image);
    });
  } finally {
    URL.revokeObjectURL(url);
  }
}

(async () => {
  let worker;
  try {
    await blobImageRoundTrip();
    const canvas = document.createElement("canvas");
    canvas.width = 420;
    canvas.height = 140;
    const context = canvas.getContext("2d");
    context.fillStyle = "white";
    context.fillRect(0, 0, canvas.width, canvas.height);
    context.fillStyle = "black";
    context.font = "bold 96px sans-serif";
    context.fillText("1234", 55, 105);
    worker = await Tesseract.createWorker(["chi_sim", "eng"], 1, {
      workerPath: "/ocr/worker.min.js",
      corePath: "/ocr/core",
      langPath: "/ocr/lang",
      gzip: true,
      workerBlobURL: false,
    });
    const result = await worker.recognize(canvas);
    if (!result || typeof result.data?.text !== "string") {
      throw new Error("Tesseract recognize returned no text result");
    }
    if (failures.length) throw new Error(failures.join("|"));
    document.title = "CSP_TESSERACT_PASS";
    document.querySelector("#result").textContent =
      "CSP_TESSERACT_PASS:" + result.data.text.trim();
  } catch (error) {
    failures.push("OCR:" + String(error));
    document.title = "CSP_TESSERACT_FAIL";
    document.querySelector("#result").textContent =
      "CSP_TESSERACT_FAIL:" + failures.join("|");
  } finally {
    if (worker) await worker.terminate();
  }
})();`;

const mimeTypes = new Map([
  [".html", "text/html; charset=utf-8"],
  [".js", "text/javascript; charset=utf-8"],
  [".wasm", "application/wasm"],
  [".gz", "application/gzip"],
  [".json", "application/json"],
]);

async function staticResponse(path, response) {
  const bytes = await readFile(path);
  response.setHeader(
    "Content-Type",
    mimeTypes.get(extname(path)) ?? "application/octet-stream",
  );
  response.end(bytes);
}

const server = createServer(async (request, response) => {
  response.setHeader("Content-Security-Policy", csp);
  response.setHeader("Cache-Control", "no-store");
  try {
    if (request.url === "/smoke.js") {
      response.setHeader("Content-Type", "text/javascript; charset=utf-8");
      response.end(script);
    } else if (request.url === "/vendor/tesseract.min.js") {
      await staticResponse(tesseractBrowser, response);
    } else if (request.url?.startsWith("/ocr/")) {
      const relative = decodeURIComponent(request.url.slice("/ocr/".length));
      const path = resolve(ocrRoot, relative);
      if (!path.startsWith(`${ocrRoot}${sep}`)) {
        response.writeHead(403).end();
        return;
      }
      await staticResponse(path, response);
    } else {
      response.setHeader("Content-Type", "text/html; charset=utf-8");
      response.end(page);
    }
  } catch (error) {
    response.writeHead(error?.code === "ENOENT" ? 404 : 500).end();
  }
});

await new Promise((resolveListen) =>
  server.listen(0, "127.0.0.1", resolveListen),
);
try {
  const address = server.address();
  if (!address || typeof address === "string") {
    throw new Error("Could not determine CSP smoke server address.");
  }
  const profile = await mkdtemp(resolve(tmpdir(), "photo-csp-chrome-"));
  const browser = spawn(chrome, [
    "--headless=new",
    "--disable-gpu",
    "--no-first-run",
    "--no-default-browser-check",
    "--remote-debugging-port=0",
    `--user-data-dir=${profile}`,
    `http://127.0.0.1:${address.port}/`,
  ], { stdio: ["ignore", "pipe", "pipe"] });
  let browserLogs = "";
  browser.stdout.on("data", chunk => {
    browserLogs += String(chunk);
  });
  browser.stderr.on("data", chunk => {
    browserLogs += String(chunk);
  });
  const wait = milliseconds =>
    new Promise(resolveWait => setTimeout(resolveWait, milliseconds));
  let socket;
  try {
    let port;
    for (let attempt = 0; attempt < 200; attempt += 1) {
      try {
        const [line] = (
          await readFile(resolve(profile, "DevToolsActivePort"), "utf8")
        ).trim().split(/\r?\n/u);
        port = Number(line);
        if (Number.isInteger(port) && port > 0) break;
      } catch {
        // Chrome has not opened its debugging endpoint yet.
      }
      await wait(50);
    }
    if (!port) throw new Error("Chrome debugging endpoint did not start.");
    let target;
    for (let attempt = 0; attempt < 200; attempt += 1) {
      const targets = await fetch(`http://127.0.0.1:${port}/json/list`)
        .then(response => response.json())
        .catch(() => []);
      target = targets.find(item =>
        item.type === "page"
        && item.url === `http://127.0.0.1:${address.port}/`
      );
      if (target?.webSocketDebuggerUrl) break;
      await wait(50);
    }
    if (!target?.webSocketDebuggerUrl) {
      throw new Error("Chrome page debugging target did not start.");
    }
    socket = new WebSocket(target.webSocketDebuggerUrl);
    await new Promise((resolveOpen, rejectOpen) => {
      socket.addEventListener("open", resolveOpen, { once: true });
      socket.addEventListener("error", rejectOpen, { once: true });
    });
    let nextId = 0;
    const pending = new Map();
    socket.addEventListener("message", event => {
      const message = JSON.parse(String(event.data));
      if (!message.id) return;
      const request = pending.get(message.id);
      if (!request) return;
      pending.delete(message.id);
      if (message.error) request.reject(new Error(message.error.message));
      else request.resolve(message.result);
    });
    const cdp = (method, params = {}) => new Promise((resolveCall, rejectCall) => {
      const id = ++nextId;
      pending.set(id, { resolve: resolveCall, reject: rejectCall });
      socket.send(JSON.stringify({ id, method, params }));
    });
    await cdp("Runtime.enable");
    const evaluated = await cdp("Runtime.evaluate", {
      expression: `new Promise((resolve) => {
        const deadline = Date.now() + 90000;
        const check = () => {
          if (document.title === "CSP_TESSERACT_PASS") {
            resolve(document.querySelector("#result").textContent);
          } else if (document.title === "CSP_TESSERACT_FAIL") {
            resolve(document.querySelector("#result").textContent);
          } else if (Date.now() >= deadline) {
            resolve("CSP_TESSERACT_TIMEOUT:" + document.documentElement.outerHTML);
          } else {
            setTimeout(check, 100);
          }
        };
        check();
      })`,
      awaitPromise: true,
      returnByValue: true,
    });
    const result = evaluated?.result?.value ?? "";
    if (!String(result).startsWith("CSP_TESSERACT_PASS:")) {
      throw new Error(
        `Chrome CSP/Tesseract smoke failed: ${result}\n${browserLogs}`,
      );
    }
  } finally {
    if (socket?.readyState === WebSocket.OPEN) socket.close();
    if (browser.exitCode === null && browser.signalCode === null) {
      browser.kill();
      const exitTimeout = new Promise(resolveTimeout => {
        setTimeout(resolveTimeout, 5_000).unref();
      });
      await Promise.race([once(browser, "exit"), exitTimeout]);
    }
    await rm(profile, {
      recursive: true,
      force: true,
      maxRetries: 5,
      retryDelay: 100,
    });
  }
  console.log(
    "Chrome CSP smoke passed: external Tesseract Worker OCR and blob image round-tripped.",
  );
  console.log(
    "Scope: Chrome only; WKWebView and WebView2 remain final packaged-app acceptance checks.",
  );
} finally {
  await new Promise((resolveClose, reject) =>
    server.close((error) => (error ? reject(error) : resolveClose())),
  );
}

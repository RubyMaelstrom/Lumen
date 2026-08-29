// Execute one WPT `.any.js` file in WPT's official JavaScript-shell test environment. The Python
// coordinator starts a fresh Lumen realm for every file and parses the single prefixed JSON line.
const fs = require("fs");
const path = require("path");

const wptRoot = process.argv[2];
const testName = process.argv[3];
if (!wptRoot || !testName) throw new Error("usage: wpt-shell.js WPT_ROOT TEST_FILE");

const queryAt = testName.indexOf("?");
const testFile = queryAt === -1 ? testName : testName.slice(0, queryAt);
const testQuery = queryAt === -1 ? "" : testName.slice(queryAt);
const testPath = path.resolve(wptRoot, testFile);
const harnessPath = path.join(wptRoot, "resources", "testharness.js");

if (!("location" in globalThis)) {
  const href = "https://web-platform.test/" + testFile + testQuery;
  globalThis.location = {
    href,
    origin: "https://web-platform.test",
    protocol: "https:",
    host: "web-platform.test",
    hostname: "web-platform.test",
    pathname: "/" + testFile,
    search: testQuery,
    hash: "",
    toString() { return this.href; }
  };
}

// A JavaScript shell has no WPT HTTP server, but data-driven `.any.js` tests still fetch static
// repository fixtures. Serve same-origin files from the pinned checkout; actual transport tests
// stay out of this manifest and run through the protocol gates instead.
const runtimeFetch = globalThis.fetch;
globalThis.fetch = function wptFixtureFetch(input, init) {
  const raw = input instanceof Request ? input.url : String(input);
  const url = new URL(raw, location.href);
  if (url.origin === location.origin) {
    const relative = decodeURIComponent(url.pathname).replace(/^\/+/, "");
    const resource = path.resolve(wptRoot, relative);
    const outside = path.relative(wptRoot, resource).startsWith("..");
    if (!outside && fs.existsSync(resource) && fs.statSync(resource).isFile()) {
      const bytes = fs.readFileSync(resource);
      const type = resource.endsWith(".json") ? "application/json" : "application/octet-stream";
      const response = new Response(bytes, { headers: { "content-type": type } });
      response.url = url.href;
      return Promise.resolve(response);
    }
  }
  return runtimeFetch(input, init);
};

function source(file) {
  return fs.readFileSync(file, "utf8") + "\n//# sourceURL=" + file.replaceAll("\\", "/") + "\n";
}

function dependencyPath(specifier) {
  const clean = specifier.split("?")[0];
  return clean.startsWith("/")
    ? path.join(wptRoot, clean.slice(1))
    : path.resolve(path.dirname(testPath), clean);
}

// testharness.js deliberately selects ShellTestEnvironment when no DOM/Worker global exists.
(0, eval)(source(harnessPath));
add_completion_callback((tests, harness) => {
  console.log("__LUMEN_WPT_H__" + JSON.stringify({
    file: testName,
    status: harness.status,
    message: harness.message || ""
  }));
  for (const test of tests) {
    console.log("__LUMEN_WPT_T__" + JSON.stringify({
      status: test.status,
      name: test.name,
      message: test.message || ""
    }));
  }
});

const testSource = source(testPath);
let executable = "";
for (const match of testSource.matchAll(/^\/\/ META: script=(.+)$/gm)) {
  executable += source(dependencyPath(match[1].trim()));
}
// Classic scripts share one global lexical environment. A single indirect eval preserves that
// relationship for top-level `const`/`let` declarations in META dependencies and the test body.
(0, eval)(executable + testSource);
